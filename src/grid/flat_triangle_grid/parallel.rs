//! Parallel grid builder

use crate::grid::flat_triangle_grid::{FlatTriangleGrid, FlatTriangleGridBuilder};
use crate::grid::parallel_grid::ParallelGrid;
use crate::traits::grid::ParallelBuilder;
use crate::traits::types::Ownership;
use mpi::{
    request::WaitGuard,
    topology::Communicator,
    traits::{Buffer, Destination, Equivalence, Source},
};
use num::Float;
use rlst::{
    dense::array::views::ArrayViewMut, rlst_dynamic_array2, Array, BaseArray, MatrixInverse,
    RandomAccessMut, RlstScalar, VectorContainer,
};
use std::collections::HashMap;

impl<T: Float + RlstScalar<Real = T> + Equivalence> ParallelBuilder<3>
    for FlatTriangleGridBuilder<T>
where
    for<'a> Array<T, ArrayViewMut<'a, T, BaseArray<T, VectorContainer<T>, 2>, 2>, 2>: MatrixInverse,
    [T]: Buffer,
{
    type ParallelGridType<'a, C: Communicator + 'a> = ParallelGrid<'a, C, FlatTriangleGrid<T>>;
    fn create_parallel_grid<'a, C: Communicator>(
        self,
        comm: &'a C,
        cell_owners: &HashMap<usize, usize>,
    ) -> ParallelGrid<'a, C, FlatTriangleGrid<T>> {
        let rank = comm.rank() as usize;
        let size = comm.size() as usize;

        let npts = self.point_indices_to_ids.len();
        let ncells = self.cell_indices_to_ids.len();

        // data used in computation
        let mut vertex_owners = vec![(-1, 0); npts];
        let mut vertex_counts = vec![0; size];
        let mut cell_indices_per_proc = vec![vec![]; size];
        let mut vertex_indices_per_proc = vec![vec![]; size];
        let mut edge_owners = HashMap::new();
        let mut edge_counts = vec![0; size];
        let mut edges_per_proc = vec![vec![]; size];

        // data to send to other processes
        let mut points_per_proc = vec![vec![]; size];
        let mut cells_per_proc = vec![vec![]; size];
        let mut point_ids_per_proc = vec![vec![]; size];
        let mut cell_ids_per_proc = vec![vec![]; size];
        let mut vertex_owners_per_proc = vec![vec![]; size];
        let mut vertex_local_indices_per_proc = vec![vec![]; size];
        let mut edge_vertices0_per_proc = vec![vec![]; size];
        let mut edge_vertices1_per_proc = vec![vec![]; size];
        let mut edge_owners_per_proc = vec![vec![]; size];
        let mut edge_local_indices_per_proc = vec![vec![]; size];
        let mut cell_owners_per_proc = vec![vec![]; size];
        let mut cell_local_indices_per_proc = vec![vec![]; size];

        for (index, id) in self.cell_indices_to_ids.iter().enumerate() {
            let owner = cell_owners[&id];
            for v in &self.cells[3 * index..3 * (index + 1)] {
                if vertex_owners[*v].0 == -1 {
                    vertex_owners[*v] = (owner as i32, vertex_counts[owner]);
                }
                if !vertex_indices_per_proc[owner].contains(v) {
                    vertex_indices_per_proc[owner].push(*v);
                    vertex_owners_per_proc[owner].push(vertex_owners[*v].0 as usize);
                    vertex_local_indices_per_proc[owner].push(vertex_owners[*v].1);
                    for i in 0..3 {
                        points_per_proc[owner].push(self.points[v * 3 + i])
                    }
                    point_ids_per_proc[owner].push(self.point_indices_to_ids[*v]);
                    vertex_counts[owner] += 1;
                }
            }
        }

        for (index, id) in self.cell_indices_to_ids.iter().enumerate() {
            let owner = cell_owners[&id];
            for e in [[1, 2], [0, 2], [0, 1]] {
                let v0 = self.cells[3 * index + e[0]];
                let v1 = self.cells[3 * index + e[1]];
                let edge = if v0 < v1 { [v0, v1] } else { [v1, v0] };
                let local_v0 = vertex_indices_per_proc[owner]
                    .iter()
                    .position(|&r| r == v0)
                    .unwrap();
                let local_v1 = vertex_indices_per_proc[owner]
                    .iter()
                    .position(|&r| r == v1)
                    .unwrap();
                let local_edge = if local_v0 < local_v1 {
                    [local_v0, local_v1]
                } else {
                    [local_v1, local_v0]
                };
                if edge_owners.get_mut(&edge).is_none() {
                    edge_owners.insert(edge, (owner, edge_counts[owner]));
                    edges_per_proc[owner].push(edge);
                    edge_vertices0_per_proc[owner].push(local_edge[0]);
                    edge_vertices1_per_proc[owner].push(local_edge[1]);
                    edge_owners_per_proc[owner].push(edge_owners[&edge].0);
                    edge_local_indices_per_proc[owner].push(edge_owners[&edge].1);
                    edge_counts[owner] += 1;
                }
            }
        }

        for index in 0..ncells {
            for p in 0..size {
                for v in &self.cells[3 * index..3 * (index + 1)] {
                    if vertex_indices_per_proc[p].contains(v) {
                        cell_indices_per_proc[p].push(index);
                        break;
                    }
                }
            }
        }

        for p in 0..size {
            for index in &cell_indices_per_proc[p] {
                let id = self.cell_indices_to_ids[*index];
                for v in &self.cells[3 * index..3 * (index + 1)] {
                    if !vertex_indices_per_proc[p].contains(v) {
                        vertex_indices_per_proc[p].push(*v);
                        vertex_owners_per_proc[p].push(vertex_owners[*v].0 as usize);
                        vertex_local_indices_per_proc[p].push(vertex_owners[*v].1);
                        for i in 0..3 {
                            points_per_proc[p].push(self.points[v * 3 + i])
                        }
                        point_ids_per_proc[p].push(self.point_indices_to_ids[*v])
                    }
                    cells_per_proc[p].push(
                        vertex_indices_per_proc[p]
                            .iter()
                            .position(|&r| r == *v)
                            .unwrap(),
                    );
                }

                for e in [[1, 2], [0, 2], [0, 1]] {
                    let v0 = self.cells[3 * index + e[0]];
                    let v1 = self.cells[3 * index + e[1]];
                    let edge = if v0 < v1 { [v0, v1] } else { [v1, v0] };
                    let local_v0 = vertex_indices_per_proc[p]
                        .iter()
                        .position(|&r| r == v0)
                        .unwrap();
                    let local_v1 = vertex_indices_per_proc[p]
                        .iter()
                        .position(|&r| r == v1)
                        .unwrap();
                    let local_edge = if local_v0 < local_v1 {
                        [local_v0, local_v1]
                    } else {
                        [local_v1, local_v0]
                    };
                    if !edges_per_proc[p].contains(&edge) {
                        edges_per_proc[p].push(edge);
                        edge_vertices0_per_proc[p].push(local_edge[0]);
                        edge_vertices1_per_proc[p].push(local_edge[1]);
                        edge_owners_per_proc[p].push(edge_owners[&edge].0);
                        edge_local_indices_per_proc[p].push(edge_owners[&edge].1);
                    }
                }

                cell_ids_per_proc[p].push(id);
                cell_owners_per_proc[p].push(cell_owners[&id]);
                cell_local_indices_per_proc[p].push(
                    cell_indices_per_proc[cell_owners[&id]]
                        .iter()
                        .position(|&r| r == *index)
                        .unwrap(),
                );
            }
        }

        mpi::request::scope(|scope| {
            for p in 1..size {
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &points_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &cells_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &point_ids_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &vertex_owners_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &vertex_local_indices_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &cell_ids_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &cell_owners_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &cell_local_indices_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &edge_owners_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &edge_local_indices_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &edge_vertices0_per_proc[p]),
                );
                let _ = WaitGuard::from(
                    comm.process_at_rank(p as i32)
                        .immediate_send(scope, &edge_vertices1_per_proc[p]),
                );
            }
        });
        self.create_internal(
            comm,
            &points_per_proc[rank],
            &cells_per_proc[rank],
            &point_ids_per_proc[rank],
            &vertex_owners_per_proc[rank],
            &vertex_local_indices_per_proc[rank],
            &cell_ids_per_proc[rank],
            &cell_owners_per_proc[rank],
            &cell_local_indices_per_proc[rank],
            &edge_owners_per_proc[rank],
            &edge_local_indices_per_proc[rank],
            &edge_vertices0_per_proc[rank],
            &edge_vertices1_per_proc[rank],
        )
    }
    fn receive_parallel_grid<C: Communicator>(
        self,
        comm: &C,
        root_rank: usize,
    ) -> ParallelGrid<'_, C, FlatTriangleGrid<T>> {
        let root_process = comm.process_at_rank(root_rank as i32);

        let (points, _status) = root_process.receive_vec::<T>();
        let (cells, _status) = root_process.receive_vec::<usize>();
        let (point_ids, _status) = root_process.receive_vec::<usize>();
        let (vertex_owners, _status) = root_process.receive_vec::<usize>();
        let (vertex_local_indices, _status) = root_process.receive_vec::<usize>();
        let (cell_ids, _status) = root_process.receive_vec::<usize>();
        let (cell_owners, _status) = root_process.receive_vec::<usize>();
        let (cell_local_indices, _status) = root_process.receive_vec::<usize>();
        let (edge_owners, _status) = root_process.receive_vec::<usize>();
        let (edge_local_indices, _status) = root_process.receive_vec::<usize>();
        let (edge_vertices0, _status) = root_process.receive_vec::<usize>();
        let (edge_vertices1, _status) = root_process.receive_vec::<usize>();
        self.create_internal(
            comm,
            &points,
            &cells,
            &point_ids,
            &vertex_owners,
            &vertex_local_indices,
            &cell_ids,
            &cell_owners,
            &cell_local_indices,
            &edge_owners,
            &edge_local_indices,
            &edge_vertices0,
            &edge_vertices1,
        )
    }
}

impl<T: Float + RlstScalar<Real = T>> FlatTriangleGridBuilder<T>
where
    for<'a> Array<T, ArrayViewMut<'a, T, BaseArray<T, VectorContainer<T>, 2>, 2>, 2>: MatrixInverse,
{
    #[allow(clippy::too_many_arguments)]
    fn create_internal<'a, C: Communicator>(
        self,
        comm: &'a C,
        points: &[T],
        cells: &[usize],
        point_ids: &[usize],
        vertex_owners: &[usize],
        vertex_local_indices: &[usize],
        cell_ids: &[usize],
        cell_owners: &[usize],
        cell_local_indices: &[usize],
        edge_owners: &[usize],
        edge_local_indices: &[usize],
        edge_vertices0: &[usize],
        edge_vertices1: &[usize],
    ) -> ParallelGrid<'a, C, FlatTriangleGrid<T>> {
        let rank = comm.rank() as usize;
        let npts = point_ids.len();
        let ncells = cell_ids.len();

        let mut coordinates = rlst_dynamic_array2!(T, [npts, 3]);
        for i in 0..npts {
            for j in 0..3 {
                *coordinates.get_mut([i, j]).unwrap() = points[i * 3 + j];
            }
        }

        let mut point_ids_to_indices = HashMap::new();
        for (index, id) in point_ids.iter().enumerate() {
            point_ids_to_indices.insert(*id, index);
        }
        let mut cell_ids_to_indices = HashMap::new();
        for (index, id) in cell_ids.iter().enumerate() {
            cell_ids_to_indices.insert(*id, index);
        }

        let mut cell_ownership = vec![];
        for index in 0..ncells {
            cell_ownership.push(if cell_owners[index] == rank {
                Ownership::Owned
            } else {
                Ownership::Ghost(cell_owners[index], cell_local_indices[index])
            });
        }
        let mut vertex_ownership = vec![];
        for index in 0..npts {
            vertex_ownership.push(if vertex_owners[index] == rank {
                Ownership::Owned
            } else {
                Ownership::Ghost(vertex_owners[index], vertex_local_indices[index])
            });
        }

        let edge_ownership = edge_owners
            .iter()
            .zip(edge_local_indices)
            .map(|(i, j)| {
                if *i == rank {
                    Ownership::Owned
                } else {
                    Ownership::Ghost(*i, *j)
                }
            })
            .collect::<Vec<_>>();
        let edges = edge_vertices0
            .iter()
            .zip(edge_vertices1)
            .map(|(i, j)| [*i, *j])
            .collect::<Vec<_>>();

        let serial_grid = FlatTriangleGrid::new(
            coordinates,
            cells,
            point_ids.to_vec(),
            point_ids_to_indices,
            cell_ids.to_vec(),
            cell_ids_to_indices,
            Some(cell_ownership),
            Some(edges),
            Some(edge_ownership),
            Some(vertex_ownership),
        );

        ParallelGrid::new(comm, serial_grid)
    }
}

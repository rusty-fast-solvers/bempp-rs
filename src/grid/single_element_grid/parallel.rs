//! Parallel grid builder

use crate::grid::parallel_grid::ParallelGrid;
use crate::grid::single_element_grid::{SingleElementGrid, SingleElementGridBuilder};
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

impl<const GDIM: usize, T: Float + RlstScalar<Real = T> + Equivalence> ParallelBuilder<GDIM>
    for SingleElementGridBuilder<GDIM, T>
where
    for<'a> Array<T, ArrayViewMut<'a, T, BaseArray<T, VectorContainer<T>, 2>, 2>, 2>: MatrixInverse,
    [T]: Buffer,
{
    type ParallelGridType<'a, C: Communicator + 'a> = ParallelGrid<'a, C, SingleElementGrid<T>>;
    fn create_parallel_grid<'a, C: Communicator>(
        self,
        comm: &'a C,
        cell_owners: &HashMap<usize, usize>,
    ) -> ParallelGrid<'a, C, SingleElementGrid<T>> {
        let rank = comm.rank() as usize;
        let size = comm.size() as usize;

        let npts = self.point_indices_to_ids.len();
        let ncells = self.cell_indices_to_ids.len();

        // data used in computation
        let mut vertex_owners = vec![(-1, 0); npts];
        let mut vertex_counts = vec![0; size];
        let mut cell_indices_per_proc = vec![vec![]; size];
        let mut vertex_indices_per_proc = vec![vec![]; size];

        // data to send to other processes
        let mut points_per_proc = vec![vec![]; size];
        let mut cells_per_proc = vec![vec![]; size];
        let mut point_ids_per_proc = vec![vec![]; size];
        let mut cell_ids_per_proc = vec![vec![]; size];
        let mut vertex_owners_per_proc = vec![vec![]; size];
        let mut vertex_local_indices_per_proc = vec![vec![]; size];
        let mut cell_owners_per_proc = vec![vec![]; size];
        let mut cell_local_indices_per_proc = vec![vec![]; size];

        for (index, id) in self.cell_indices_to_ids.iter().enumerate() {
            let owner = cell_owners[&id];
            for v in &self.cells[self.points_per_cell * index..self.points_per_cell * (index + 1)] {
                if vertex_owners[*v].0 == -1 {
                    vertex_owners[*v] = (owner as i32, vertex_counts[owner]);
                }
                if !vertex_indices_per_proc[owner].contains(v) {
                    vertex_indices_per_proc[owner].push(*v);
                    vertex_owners_per_proc[owner].push(vertex_owners[*v].0 as usize);
                    vertex_local_indices_per_proc[owner].push(vertex_owners[*v].1);
                    for i in 0..GDIM {
                        points_per_proc[owner].push(self.points[v * GDIM + i])
                    }
                    point_ids_per_proc[owner].push(self.point_indices_to_ids[*v]);
                    vertex_counts[owner] += 1;
                }
            }
        }

        for index in 0..ncells {
            for p in 0..size {
                for v in
                    &self.cells[self.points_per_cell * index..self.points_per_cell * (index + 1)]
                {
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
                for v in
                    &self.cells[self.points_per_cell * index..self.points_per_cell * (index + 1)]
                {
                    if !vertex_indices_per_proc[p].contains(v) {
                        vertex_indices_per_proc[p].push(*v);
                        vertex_owners_per_proc[p].push(vertex_owners[*v].0 as usize);
                        vertex_local_indices_per_proc[p].push(vertex_owners[*v].1);
                        for i in 0..GDIM {
                            points_per_proc[p].push(self.points[v * GDIM + i])
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

        println!("cells_per_proc = {cells_per_proc:?}");

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
        )
    }
    fn receive_parallel_grid<C: Communicator>(
        self,
        comm: &C,
        root_rank: usize,
    ) -> ParallelGrid<'_, C, SingleElementGrid<T>> {
        let root_process = comm.process_at_rank(root_rank as i32);

        let (points, _status) = root_process.receive_vec::<T>();
        let (cells, _status) = root_process.receive_vec::<usize>();
        let (point_ids, _status) = root_process.receive_vec::<usize>();
        let (vertex_owners, _status) = root_process.receive_vec::<usize>();
        let (vertex_local_indices, _status) = root_process.receive_vec::<usize>();
        let (cell_ids, _status) = root_process.receive_vec::<usize>();
        let (cell_owners, _status) = root_process.receive_vec::<usize>();
        let (cell_local_indices, _status) = root_process.receive_vec::<usize>();
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
        )
    }
}

impl<const GDIM: usize, T: Float + RlstScalar<Real = T>> SingleElementGridBuilder<GDIM, T>
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
    ) -> ParallelGrid<'a, C, SingleElementGrid<T>> {
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

        let mut edge_ownership = vec![];
        let mut edges = vec![];

        println!("[{rank}] point_ids = {point_ids:?}");
        println!("[{rank}] vertex_local_indices = {vertex_local_indices:?}");


        println!("[{rank}] vertex_ownership = {vertex_ownership:?}");
        println!("[{rank}] cells = {cells:?}");
        println!("[{rank}] cell_ownership = {cell_ownership:?}");

        let serial_grid = SingleElementGrid::new(
            coordinates,
            cells,
            self.element_data.0,
            self.element_data.1,
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

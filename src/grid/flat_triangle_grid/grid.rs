//! Flat triangle grid

use crate::element::ciarlet::{lagrange, CiarletElement};
use crate::element::reference_cell;
// use crate::grid::traits::{Geometry, GeometryEvaluator, Grid, Topology};
use crate::traits::grid::Grid;
use crate::traits::types::{CellLocalIndexPair, Ownership, ReferenceCellType};
use crate::types::{IntegerArray2, RealScalar};
use itertools::Itertools;
use num::Float;
use rlst::prelude::*;
use rlst::{rlst_static_array, LinAlg};
use rlst::{rlst_static_type, DynamicArray};
use rlst::{RlstScalar, UnsafeRandomAccessByValue};
use std::collections::HashMap;
use std::marker::PhantomData;

/// A flat triangle grid
pub struct FlatTriangleGrid<T: RealScalar> {
    // Geometry information
    vertices: DynamicArray<T, 2>,
    cells: IntegerArray2,
    midpoints: Vec<rlst_static_type!(T, 3)>,
    diameters: Vec<T>,
    volumes: Vec<T>,
    normals: Vec<rlst_static_type!(T, 3)>,
    jacobians: Vec<rlst_static_type!(T, 3, 2)>,

    // Topological information
    cell_edges: IntegerArray2,
    edge_to_vertices: IntegerArray2,
    edge_to_cells: HashMap<usize, Vec<(usize, usize)>>,
    vertex_to_cells: HashMap<usize, Vec<(usize, usize)>>,
    entity_types: Vec<ReferenceCellType>,

    // Point, edge and cell ids
    point_indices_to_ids: Vec<usize>,
    point_ids_to_indices: HashMap<usize, usize>,
    cell_indices_to_ids: Vec<usize>,
    cell_ids_to_indices: HashMap<usize, usize>,
}

impl<T: RealScalar> FlatTriangleGrid<T> {
    /// Create a flat triangle grid
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vertices: &[T],
        cells: &[usize],
        vertex_ids: &Vec<usize>,
        cell_ids: &Vec<usize>,
    ) -> Self {
        assert_eq!(vertices.len() % 3, 0);
        assert_eq!(cells.len() % 3, 0);
        let nvertices = vertices.len() / 3;
        let ncells = cells.len() / 3;

        let vertices = {
            let tmp = rlst_dynamic_array2!(T, [3, nvertices]);
            tmp.data_mut().clone_from_slice(vertices);
            tmp
        };
        let cells = IntegerArray2::new_from_slice(cells, [3, ncells]);
        // Compute geometry

        let mut midpoints = rlst_dynamic_array2!(T, [3, ncells]);
        let mut diameters = rlst_dynamic_array1!(T, [ncells]);
        let mut volumes = rlst_dynamic_array1!(T, [ncells]);
        let mut normals = rlst_dynamic_array2!(T, [3, ncells]);
        let mut jacobians = rlst_dynamic_array2!(T, [3, 2 * ncells]);

        let mut a = rlst_static_array!(T, 3);
        let mut b = rlst_static_array!(T, 3);
        let mut c = rlst_static_array!(T, 3);

        let mut v0 = rlst_static_array!(T, 3);
        let mut v1 = rlst_static_array!(T, 3);
        let mut v2 = rlst_static_array!(T, 3);

        for (midpoint, diameter, volume, normal, jacobian, cell) in itertools::izip!(
            midpoints.col_iter_mut(),
            diameters.iter_mut(),
            volumes.iter_mut(),
            normals.col_iter_mut(),
            jacobians.col_iter_mut().tuples::<(_, _)>(),
            cells.col_iter(),
        ) {
            let cell: [usize; 3] = cell.try_into().unwrap();

            v0.fill_from(vertices.view().slice(1, cell[0]));
            v1.fill_from(vertices.view().slice(1, cell[1]));
            v2.fill_from(vertices.view().slice(1, cell[2]));

            midpoint.fill_from(
                (v0.view() + v1.view() + v2.view()).scalar_mul(T::from(1.0 / 3.0).unwrap()),
            );

            a.fill_from(v1.view() - v0.view());
            b.fill_from(v2.view() - v0.view());
            c.fill_from(v2.view() - v1.view());
            jacobian.0.fill_from(a.view());
            jacobian.1.fill_from(b.view());

            a.cross(b.view(), normal.view_mut());

            let normal_length = normal.view().norm_2();
            normals.scale_inplace(T::one() / normal_length);

            *volume = normal_length / T::from(2.0).unwrap();
            *diameter = compute_diameter_triangle(v0.view(), v1.view(), v2.view());
        }

        // Compute topology
        let entity_types = vec![
            ReferenceCellType::Point,
            ReferenceCellType::Interval,
            ReferenceCellType::Triangle,
        ];

        let mut edge_indices = HashMap::<(usize, usize), usize>::new();

        let ref_conn = &reference_cell::connectivity(ReferenceCellType::Triangle)[1];
        let edge_to_cells: HashMap<usize, Vec<CellLocalIndexPair<usize>>> = HashMap::new();
        let edge_to_vertices = Vec::<usize>::new();
        let vertex_to_cells = HashMap::<usize, Vec<CellLocalIndexPair<usize>>>::new();

        for (cell_index, cell) in cells.col_iter().enumerate() {
            // Associate cell to adjacent vertices

            for (vertex_local_index, vertex) in cell.iter().enumerate() {
                if let Some(vertex_pair_list) = vertex_to_cells.get(vertex) {
                    vertex_pair_list.push(CellLocalIndexPair::new(cell_index, vertex_local_index));
                } else {
                    vertex_to_cells.insert(
                        *vertex,
                        vec![CellLocalIndexPair::new(cell_index, vertex_local_index)],
                    );
                }
            }

            for (local_index, rc) in ref_conn.iter().enumerate() {
                let first = cell[rc[0][0]];
                let second = cell[rc[0][1]];
                if first > second {
                    std::mem::swap(&mut first, &mut second);
                }
                if let Some(edge_index) = edge_indices.get((first, second)) {
                    edge_to_cells[edge_index]
                        .push(CellLocalIndexPair::new(cell_index, local_index));
                } else {
                    let edge_index = edge_indices.len();
                    edge_indices.insert((first, second), edge_index);
                    edge_to_cells.insert(
                        edge_index,
                        vec![CellLocalIndexPair::new(cell_index, local_index)],
                    );
                    edge_to_vertices.push(first);
                    edge_to_vertices.push(second);
                }
            }
        }

        let edge_to_vertices = IntegerArray2::new_from_slice(
            edge_to_vertices.as_slice(),
            [2, edge_to_vertices.len() / 2],
        );

        // // Topological information
        // cell_edges: IntegerArray2,
        // edge_to_vertices: IntegerArray2,
        // edge_to_cells: HashMap<usize, Vec<(usize, usize)>>,
        // vertex_to_cells: HashMap<usize, Vec<(usize, usize)>>,
        // entity_types: Vec<ReferenceCellType>,

        // for cell_i in 0..ncells {

        //     v0.fill_from(coordinates.view().slice(1, cells[3 * cell_i]));
        //     v1.fill_from(coordinates.view().slice(1, cells[3 * cell_i + 1]));
        //     v2.fill_from(coordinates.view().slice(1, cells[3 * cell_i + 2]));

        // midpoints[cell_i].fill_from(
        //     (v0.view() + v1.view() + v2.view()).scalar_mul(T::from(1.0 / 3.0).unwrap()),
        // );

        //     a.fill_from(v1.view() - v0.view());
        //     b.fill_from(v2.view() - v0.view());
        //     c.fill_from(v2.view() - v1.view());
        //     jacobians[cell_i].view_mut().slice(1, 0).fill_from(a.view());
        //     jacobians[cell_i].view_mut().slice(1, 1).fill_from(b.view());

        //     a.cross(b.view(), normals[cell_i].view_mut());

        //     let normal_length = normals[cell_i].view().norm_2();
        //     normals[cell_i].scale_inplace(T::one() / normal_length);

        //     volumes.push(normal_length / T::from(2.0).unwrap());
        //     diameters.push(compute_diameter_triangle(v0.view(), v1.view(), v2.view()));
        // }

        // // Compute topology
        // let entity_types = vec![
        //     ReferenceCellType::Point,
        //     ReferenceCellType::Interval,
        //     ReferenceCellType::Triangle,
        // ];

        // let mut cells_to_entities = vec![vec![vec![]; ncells]; 3];
        // let mut entities_to_cells = vec![vec![]; 3];
        // let mut entities_to_vertices = vec![vec![]; 2];

        // entities_to_cells[2] = vec![vec![]; ncells];
        // entities_to_vertices[0] = (0..nvertices).map(|i| vec![i]).collect::<Vec<_>>();
        // entities_to_cells[0] = vec![vec![]; nvertices];

        // for (cell_i, i) in index_map.iter_mut().enumerate() {
        //     let cell = &cells[3 * cell_i..3 * cell_i + 3];
        //     *i = cell_i;
        //     for (local_index, &v) in cell.iter().enumerate() {
        //         entities_to_cells[0][v].push(CellLocalIndexPair::new(cell_i, local_index));
        //     }
        //     entities_to_cells[2][cell_i] = vec![CellLocalIndexPair::new(cell_i, 0)];
        //     cells_to_entities[0][cell_i].extend_from_slice(cell);
        //     cells_to_entities[2][cell_i] = vec![cell_i];
        // }

        // let mut edge_indices = HashMap::new();
        // let ref_conn = &reference_cell::connectivity(ReferenceCellType::Triangle)[1];
        // for cell_i in 0..ncells {
        //     let cell = &cells[3 * cell_i..3 * (cell_i + 1)];
        //     for (local_index, rc) in ref_conn.iter().enumerate() {
        //         let mut first = cell[rc[0][0]];
        //         let mut second = cell[rc[0][1]];
        //         if first > second {
        //             std::mem::swap(&mut first, &mut second);
        //         }
        //         if let Some(edge_index) = edge_indices.get(&(first, second)) {
        //             cells_to_entities[1][cell_i].push(*edge_index);
        //             entities_to_cells[1][*edge_index]
        //                 .push(CellLocalIndexPair::new(cell_i, local_index));
        //         } else {
        //             let edge_index = entities_to_vertices[1].len();
        //             edge_indices.insert((first, second), edge_index);
        //             cells_to_entities[1][cell_i].push(entities_to_vertices[1].len());
        //             entities_to_cells[1].push(vec![CellLocalIndexPair::new(cell_i, local_index)]);
        //             entities_to_vertices[1].push(vec![first, second]);
        //         }
        //     }
        // }
        // Self {
        //     index_map,
        //     coordinates,
        //     element,
        //     midpoints,
        //     diameters,
        //     volumes,
        //     normals,
        //     jacobians,
        //     cell_indices,
        //     entities_to_vertices,
        //     cells_to_entities,
        //     entities_to_cells,
        //     entity_types,
        //     point_indices_to_ids,
        //     point_ids_to_indices,
        //     cell_indices_to_ids,
        //     cell_ids_to_indices,
        // }
    }
}

// impl<T: LinAlg + Float + RlstScalar<Real = T>> Grid for FlatTriangleGrid<T> {
//     type T = T;
//     type Topology = Self;
//     type Geometry = Self;

//     fn topology(&self) -> &Self::Topology {
//         self
//     }

//     fn geometry(&self) -> &Self::Geometry {
//         self
//     }

//     fn is_serial(&self) -> bool {
//         true
//     }
// }

// impl<T: LinAlg + Float + RlstScalar<Real = T>> Geometry for FlatTriangleGrid<T> {
//     type IndexType = usize;
//     type T = T;
//     type Element = CiarletElement<T>;
//     type Evaluator<'a> = GeometryEvaluatorFlatTriangle<'a, T> where Self: 'a;

//     fn dim(&self) -> usize {
//         3
//     }

//     fn index_map(&self) -> &[usize] {
//         &self.index_map
//     }

//     fn coordinate(&self, point_index: usize, coord_index: usize) -> Option<&Self::T> {
//         self.coordinates.get([coord_index, point_index])
//     }

//     fn point_count(&self) -> usize {
//         self.coordinates.shape()[1]
//     }

//     fn cell_points(&self, index: usize) -> Option<&[usize]> {
//         if let Some(c) = self.cells_to_entities[0].get(index) {
//             Some(&c)
//         } else {
//             None
//         }
//     }

//     fn cell_count(&self) -> usize {
//         self.cells_to_entities[0].len()
//     }

//     fn cell_element(&self, index: usize) -> Option<&Self::Element> {
//         if index < self.cells_to_entities[0].len() {
//             Some(&self.element)
//         } else {
//             None
//         }
//     }

//     fn element_count(&self) -> usize {
//         1
//     }
//     fn element(&self, i: usize) -> Option<&Self::Element> {
//         if i == 0 {
//             Some(&self.element)
//         } else {
//             None
//         }
//     }
//     fn cell_indices(&self, i: usize) -> Option<&[usize]> {
//         if i == 0 {
//             Some(&self.cell_indices)
//         } else {
//             None
//         }
//     }

//     fn midpoint(&self, index: usize, point: &mut [Self::T]) {
//         point.copy_from_slice(self.midpoints[index].data());
//     }

//     fn diameter(&self, index: usize) -> Self::T {
//         self.diameters[index]
//     }
//     fn volume(&self, index: usize) -> Self::T {
//         self.volumes[index]
//     }

//     fn get_evaluator<'a>(&'a self, points: &'a [Self::T]) -> GeometryEvaluatorFlatTriangle<'a, T> {
//         GeometryEvaluatorFlatTriangle::<T>::new(self, points)
//     }

//     fn point_index_to_id(&self, index: usize) -> usize {
//         self.point_indices_to_ids[index]
//     }
//     fn cell_index_to_id(&self, index: usize) -> usize {
//         self.cell_indices_to_ids[index]
//     }
//     fn point_id_to_index(&self, id: usize) -> usize {
//         self.point_ids_to_indices[&id]
//     }
//     fn cell_id_to_index(&self, id: usize) -> usize {
//         self.cell_ids_to_indices[&id]
//     }
// }

// /// Geometry evaluator for a flat triangle grid
// pub struct GeometryEvaluatorFlatTriangle<'a, T: LinAlg + Float + RlstScalar<Real = T>> {
//     grid: &'a FlatTriangleGrid<T>,
//     points: SliceArray<'a, T, 2>,
// }

// impl<'a, T: LinAlg + Float + RlstScalar<Real = T>> GeometryEvaluatorFlatTriangle<'a, T> {
//     /// Create a geometry evaluator
//     fn new(grid: &'a FlatTriangleGrid<T>, points: &'a [T]) -> Self {
//         let tdim = reference_cell::dim(grid.element.cell_type());
//         assert_eq!(points.len() % tdim, 0);
//         let npoints = points.len() / tdim;
//         Self {
//             grid,
//             points: rlst_array_from_slice2!(points, [tdim, npoints]),
//         }
//     }
// }

// impl<'a, T: LinAlg + Float + RlstScalar<Real = T>> GeometryEvaluator
//     for GeometryEvaluatorFlatTriangle<'a, T>
// {
//     type T = T;

//     fn point_count(&self) -> usize {
//         self.points.shape()[0]
//     }

//     fn compute_points(&self, cell_index: usize, points: &mut [T]) {
//         let jacobian = &self.grid.jacobians[cell_index];
//         let npts = self.points.shape()[1];
//         let mut point = rlst_static_array!(T, 3);
//         for point_index in 0..npts {
//             point.fill_from(
//                 self.grid
//                     .coordinates
//                     .view()
//                     .slice(1, self.grid.cells_to_entities[0][cell_index][0])
//                     + jacobian
//                         .view()
//                         .slice(1, 0)
//                         .scalar_mul(self.points[[0, point_index]])
//                     + jacobian
//                         .view()
//                         .slice(1, 1)
//                         .scalar_mul(self.points[[1, point_index]]),
//             );
//             points[3 * point_index..3 * point_index + 3].copy_from_slice(point.data());
//         }
//     }

//     fn compute_jacobians(&self, cell_index: usize, jacobians: &mut [T]) {
//         let npts = self.points.shape()[1];
//         for index in 0..npts {
//             jacobians[6 * index..6 * index + 6]
//                 .copy_from_slice(self.grid.jacobians[cell_index].data());
//         }
//     }

//     fn compute_normals(&self, cell_index: usize, normals: &mut [T]) {
//         let npts = self.points.shape()[1];
//         for index in 0..npts {
//             normals[3 * index..3 * index + 3].copy_from_slice(self.grid.normals[cell_index].data());
//         }
//     }
// }

// impl<T: LinAlg + Float + RlstScalar<Real = T>> Topology for FlatTriangleGrid<T> {
//     type IndexType = usize;

//     fn dim(&self) -> usize {
//         2
//     }
//     fn index_map(&self) -> &[usize] {
//         &self.index_map
//     }
//     fn entity_count(&self, etype: ReferenceCellType) -> usize {
//         if self.entity_types.contains(&etype) {
//             self.entities_to_cells[reference_cell::dim(etype)].len()
//         } else {
//             0
//         }
//     }
//     fn entity_count_by_dim(&self, dim: usize) -> usize {
//         self.entity_count(self.entity_types[dim])
//     }
//     fn cell(&self, index: usize) -> Option<&[usize]> {
//         if index < self.cells_to_entities[2].len() {
//             Some(&self.cells_to_entities[2][index])
//         } else {
//             None
//         }
//     }
//     fn cell_type(&self, index: usize) -> Option<ReferenceCellType> {
//         if index < self.cells_to_entities[2].len() {
//             Some(self.entity_types[2])
//         } else {
//             None
//         }
//     }

//     fn entity_types(&self, dim: usize) -> &[ReferenceCellType] {
//         &self.entity_types[dim..dim + 1]
//     }

//     fn cell_ownership(&self, _index: usize) -> Ownership {
//         Ownership::Owned
//     }
//     fn vertex_ownership(&self, _index: usize) -> Ownership {
//         Ownership::Owned
//     }
//     fn edge_ownership(&self, _index: usize) -> Ownership {
//         Ownership::Owned
//     }
//     fn cell_to_entities(&self, index: usize, dim: usize) -> Option<&[usize]> {
//         if dim <= 2 && index < self.cells_to_entities[dim].len() {
//             Some(&self.cells_to_entities[dim][index])
//         } else {
//             None
//         }
//     }
//     fn entity_to_cells(&self, dim: usize, index: usize) -> Option<&[CellLocalIndexPair<usize>]> {
//         if dim <= 2 && index < self.entities_to_cells[dim].len() {
//             Some(&self.entities_to_cells[dim][index])
//         } else {
//             None
//         }
//     }

//     fn entity_to_flat_cells(
//         &self,
//         dim: usize,
//         index: Self::IndexType,
//     ) -> Option<&[CellLocalIndexPair<usize>]> {
//         self.entity_to_cells(dim, index)
//     }

//     fn entity_vertices(&self, dim: usize, index: usize) -> Option<&[usize]> {
//         if dim == 2 {
//             self.cell_to_entities(index, 0)
//         } else if dim < 2 && index < self.entities_to_vertices[dim].len() {
//             Some(&self.entities_to_vertices[dim][index])
//         } else {
//             None
//         }
//     }

//     fn vertex_index_to_id(&self, index: usize) -> usize {
//         self.point_indices_to_ids[index]
//     }
//     fn cell_index_to_id(&self, index: usize) -> usize {
//         self.cell_indices_to_ids[index]
//     }
//     fn vertex_id_to_index(&self, id: usize) -> usize {
//         self.point_ids_to_indices[&id]
//     }
//     fn edge_id_to_index(&self, id: usize) -> usize {
//         self.edge_ids_to_indices[&id]
//     }
//     fn edge_index_to_id(&self, index: usize) -> usize {
//         self.edge_indices_to_ids[index]
//     }
//     fn cell_id_to_index(&self, id: usize) -> usize {
//         self.cell_ids_to_indices[&id]
//     }
//     fn face_index_to_flat_index(&self, index: usize) -> usize {
//         index
//     }
//     fn face_flat_index_to_index(&self, index: usize) -> usize {
//         index
//     }
//     fn cell_types(&self) -> &[ReferenceCellType] {
//         &[ReferenceCellType::Triangle]
//     }
// }

/// Compute the diameter of a triangle
fn compute_diameter_triangle<
    T: Float + Float + RlstScalar<Real = T>,
    ArrayImpl: UnsafeRandomAccessByValue<1, Item = T> + Shape<1>,
>(
    v0: Array<T, ArrayImpl, 1>,
    v1: Array<T, ArrayImpl, 1>,
    v2: Array<T, ArrayImpl, 1>,
) -> T {
    let a = (v0.view() - v1.view()).norm_2();
    let b = (v0 - v2.view()).norm_2();
    let c = (v1 - v2).norm_2();
    RlstScalar::sqrt((b + c - a) * (a + c - b) * (a + b - c) / (a + b + c))
}

// #[cfg(test)]
// mod test {
//     use crate::traits::grid::{Geometry, Topology};

//     use super::*;
//     use approx::*;
//     use rlst::{
//         assert_array_relative_eq, rlst_dynamic_array2, rlst_dynamic_array3, RandomAccessMut,
//         RawAccessMut,
//     };

//     fn example_grid_flat() -> FlatTriangleGrid<f64> {
//         //! Create a flat test grid
//         let mut points = rlst_dynamic_array2!(f64, [3, 4]);
//         points[[0, 0]] = 0.0;
//         points[[1, 0]] = 0.0;
//         points[[2, 0]] = 0.0;
//         points[[0, 1]] = 1.0;
//         points[[1, 1]] = 0.0;
//         points[[2, 1]] = 0.0;
//         points[[0, 2]] = 1.0;
//         points[[1, 2]] = 1.0;
//         points[[2, 2]] = 0.0;
//         points[[0, 3]] = 0.0;
//         points[[1, 3]] = 1.0;
//         points[[2, 3]] = 0.0;
//         let cells = vec![0, 1, 2, 0, 2, 3];
//         FlatTriangleGrid::new(
//             points,
//             &cells,
//             vec![0, 1, 2, 3],
//             HashMap::from([(0, 0), (1, 1), (2, 2), (3, 3)]),
//             vec![0, 1],
//             HashMap::from([(0, 0), (1, 1)]),
//         )
//     }

//     fn example_grid_3d() -> FlatTriangleGrid<f64> {
//         //! Create a non-flat test grid
//         let mut points = rlst_dynamic_array2!(f64, [3, 4]);
//         points[[0, 0]] = 0.0;
//         points[[1, 0]] = 0.0;
//         points[[2, 0]] = 0.0;
//         points[[0, 1]] = 1.0;
//         points[[1, 1]] = 0.0;
//         points[[2, 1]] = 1.0;
//         points[[0, 2]] = 1.0;
//         points[[1, 2]] = 1.0;
//         points[[2, 2]] = 0.0;
//         points[[0, 3]] = 0.0;
//         points[[1, 3]] = 1.0;
//         points[[2, 3]] = 0.0;
//         let cells = vec![0, 1, 2, 0, 2, 3];
//         FlatTriangleGrid::new(
//             points,
//             &cells,
//             vec![0, 1, 2, 3],
//             HashMap::from([(0, 0), (1, 1), (2, 2), (3, 3)]),
//             vec![0, 1],
//             HashMap::from([(0, 0), (1, 1)]),
//         )
//     }

//     fn triangle_points() -> Array<f64, BaseArray<f64, VectorContainer<f64>, 2>, 2> {
//         //! Create a set of points ins1de the re1erence triangle
//         let mut points = rlst_dynamic_array2!(f64, [2, 2]);
//         *points.get_mut([0, 0]).unwrap() = 0.2;
//         *points.get_mut([1, 0]).unwrap() = 0.5;
//         *points.get_mut([0, 1]).unwrap() = 0.6;
//         *points.get_mut([1, 1]).unwrap() = 0.1;
//         points
//     }

//     #[test]
//     fn test_cell_points() {
//         //! Test that the cell points are correct
//         let g = example_grid_flat();
//         for (cell_i, points) in [
//             vec![
//                 vec![0.0, 0.0, 0.0],
//                 vec![1.0, 0.0, 0.0],
//                 vec![1.0, 1.0, 0.0],
//             ],
//             vec![
//                 vec![0.0, 0.0, 0.0],
//                 vec![1.0, 1.0, 0.0],
//                 vec![0.0, 1.0, 0.0],
//             ],
//         ]
//         .iter()
//         .enumerate()
//         {
//             let vs = g.cell_points(cell_i).unwrap();
//             for (p_i, point) in points.iter().enumerate() {
//                 for (c_i, coord) in point.iter().enumerate() {
//                     assert_relative_eq!(
//                         *coord,
//                         *g.coordinate(vs[p_i], c_i).unwrap(),
//                         epsilon = 1e-12
//                     );
//                 }
//             }
//         }
//     }

//     #[test]
//     fn test_compute_point_flat() {
//         //! Test the compute_point function of an evaluator
//         let g = example_grid_flat();
//         let points = triangle_points();

//         let evaluator = g.get_evaluator(points.data());
//         let mut mapped_points = rlst_dynamic_array2!(f64, [3, points.shape()[1]]);
//         for (cell_i, pts) in [
//             vec![vec![0.7, 0.5, 0.0], vec![0.7, 0.1, 0.0]],
//             vec![vec![0.2, 0.7, 0.0], vec![0.6, 0.7, 0.0]],
//         ]
//         .iter()
//         .enumerate()
//         {
//             evaluator.compute_points(cell_i, mapped_points.data_mut());
//             for (point_i, point) in pts.iter().enumerate() {
//                 for (i, j) in point.iter().enumerate() {
//                     assert_relative_eq!(mapped_points[[i, point_i]], *j, epsilon = 1e-12);
//                 }
//             }
//         }
//     }

//     #[test]
//     fn test_compute_point_3d() {
//         //! Test the compute_point function of an evaluator
//         let g = example_grid_3d();
//         let points = triangle_points();
//         let evaluator = g.get_evaluator(points.data());

//         let mut mapped_points = rlst_dynamic_array2!(f64, [3, points.shape()[1]]);
//         for (cell_i, pts) in [
//             vec![vec![0.7, 0.5, 0.2], vec![0.7, 0.1, 0.6]],
//             vec![vec![0.2, 0.7, 0.0], vec![0.6, 0.7, 0.0]],
//         ]
//         .iter()
//         .enumerate()
//         {
//             evaluator.compute_points(cell_i, mapped_points.data_mut());
//             for (point_i, point) in pts.iter().enumerate() {
//                 for (i, j) in point.iter().enumerate() {
//                     assert_relative_eq!(mapped_points[[i, point_i]], *j, epsilon = 1e-12);
//                 }
//             }
//         }
//     }

//     #[test]
//     fn test_compute_jacobian_3d() {
//         //! Test the compute_jacobian function of an evaluator
//         let g = example_grid_3d();
//         let points = triangle_points();
//         let evaluator = g.get_evaluator(points.data());

//         let mut computed_jacobians = rlst_dynamic_array3!(f64, [3, 2, points.shape()[1]]);
//         let mut expected = rlst_dynamic_array3!(f64, [3, 2, 2]);

//         // First cell, first col

//         expected[[0, 0, 0]] = 1.0;
//         expected[[1, 0, 0]] = 0.0;
//         expected[[2, 0, 0]] = 1.0;

//         // First cell, second col

//         expected[[0, 1, 0]] = 1.0;
//         expected[[1, 1, 0]] = 1.0;
//         expected[[2, 1, 0]] = 0.0;

//         // Second cell, first col,

//         expected[[0, 0, 1]] = 1.0;
//         expected[[1, 0, 1]] = 1.0;
//         expected[[2, 0, 1]] = 0.0;

//         // Second point, second col

//         expected[[0, 1, 1]] = 0.0;
//         expected[[1, 1, 1]] = 1.0;
//         expected[[2, 1, 1]] = 0.0;

//         for cell_i in 0..2 {
//             evaluator.compute_jacobians(cell_i, computed_jacobians.data_mut());
//             for point_i in 0..points.shape()[1] {
//                 let jac = computed_jacobians.view().slice(2, point_i);
//                 assert_array_relative_eq!(jac, expected.view().slice(2, cell_i), 1E-12);
//             }
//         }
//     }

//     #[test]
//     fn test_compute_normal_3d() {
//         //! Test the compute_normal function of an evaluator
//         let g = example_grid_3d();
//         let points = triangle_points();
//         let evaluator = g.get_evaluator(points.data());

//         let mut computed_normals = rlst_dynamic_array2!(f64, [3, points.shape()[1]]);
//         let mut expected = rlst_dynamic_array2!(f64, [3, 2]);

//         expected[[0, 0]] = -1.0;
//         expected[[1, 0]] = 1.0;
//         expected[[2, 0]] = 1.0;

//         expected[[0, 1]] = 0.0;
//         expected[[1, 1]] = 0.0;
//         expected[[2, 1]] = 1.0;

//         expected
//             .view_mut()
//             .slice(1, 0)
//             .scale_inplace(1.0 / f64::sqrt(3.0));

//         for cell_i in 0..2 {
//             evaluator.compute_normals(cell_i, computed_normals.data_mut());
//             for point_i in 0..points.shape()[1] {
//                 assert_array_relative_eq!(
//                     computed_normals.view().slice(1, point_i),
//                     expected.view().slice(1, cell_i),
//                     1E-12
//                 );
//             }
//         }
//     }

//     #[test]
//     fn test_midpoint_flat() {
//         //! Test midpoints
//         let g = example_grid_flat();

//         let mut midpoint = vec![0.0; 3];
//         for (cell_i, point) in [
//             vec![2.0 / 3.0, 1.0 / 3.0, 0.0],
//             vec![1.0 / 3.0, 2.0 / 3.0, 0.0],
//         ]
//         .iter()
//         .enumerate()
//         {
//             g.midpoint(cell_i, &mut midpoint);
//             for (i, j) in midpoint.iter().zip(point) {
//                 assert_relative_eq!(*i, *j, epsilon = 1e-12);
//             }
//         }
//     }

//     #[test]
//     fn test_midpoint_3d() {
//         //! Test midpoints
//         let g = example_grid_3d();

//         let mut midpoint = vec![0.0; 3];
//         for (cell_i, point) in [
//             vec![2.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0],
//             vec![1.0 / 3.0, 2.0 / 3.0, 0.0],
//         ]
//         .iter()
//         .enumerate()
//         {
//             g.midpoint(cell_i, &mut midpoint);
//             for (i, j) in midpoint.iter().zip(point) {
//                 assert_relative_eq!(*i, *j, epsilon = 1e-12);
//             }
//         }
//     }

//     #[test]
//     fn test_counts() {
//         //! Test the entity counts
//         let g = example_grid_flat();
//         assert_eq!(Topology::dim(&g), 2);
//         assert_eq!(Geometry::dim(&g), 3);
//         assert_eq!(g.entity_count(ReferenceCellType::Point), 4);
//         assert_eq!(g.entity_count(ReferenceCellType::Interval), 5);
//         assert_eq!(g.entity_count(ReferenceCellType::Triangle), 2);

//         assert_eq!(g.point_count(), 4);
//         assert_eq!(g.cell_count(), 2);
//     }

//     #[test]
//     fn test_cell_entities_vertices() {
//         //! Test the cell vertices
//         let g = example_grid_3d();
//         for (i, vertices) in [[0, 1, 2], [0, 2, 3]].iter().enumerate() {
//             let c = g.cell_to_entities(i, 0).unwrap();
//             assert_eq!(c.len(), 3);
//             assert_eq!(c, vertices);
//         }
//     }

//     #[test]
//     fn test_cell_entities_edges() {
//         //! Test the cell edges
//         let g = example_grid_3d();
//         for (i, edges) in [[0, 1, 2], [3, 4, 1]].iter().enumerate() {
//             let c = g.cell_to_entities(i, 1).unwrap();
//             assert_eq!(c.len(), 3);
//             assert_eq!(c, edges);
//         }
//     }

//     #[test]
//     fn test_cell_entities_cells() {
//         //! Test the cells
//         let g = example_grid_3d();
//         for i in 0..2 {
//             let c = g.cell_to_entities(i, 2).unwrap();
//             assert_eq!(c.len(), 1);
//             assert_eq!(c[0], i);
//         }
//     }

//     #[test]
//     fn test_entities_to_cells_vertices() {
//         //! Test the cell-to-vertex connectivity
//         let g = example_grid_3d();
//         let c_to_e = (0..g.entity_count(ReferenceCellType::Triangle))
//             .map(|i| g.cell_to_entities(i, 0).unwrap())
//             .collect::<Vec<_>>();
//         let e_to_c = (0..g.entity_count(ReferenceCellType::Point))
//             .map(|i| {
//                 g.entity_to_cells(0, i)
//                     .unwrap()
//                     .iter()
//                     .map(|x| x.cell)
//                     .collect::<Vec<_>>()
//             })
//             .collect::<Vec<_>>();

//         for (i, cell) in c_to_e.iter().enumerate() {
//             for v in *cell {
//                 assert!(e_to_c[*v].contains(&i));
//             }
//         }
//         for (i, cells) in e_to_c.iter().enumerate() {
//             for c in cells {
//                 assert!(c_to_e[*c].contains(&i));
//             }
//         }
//     }

//     #[test]
//     fn test_entities_to_cells_edges() {
//         //! Test the cell-to-edge connectivity
//         let g = example_grid_3d();
//         let c_to_e = (0..g.entity_count(ReferenceCellType::Triangle))
//             .map(|i| g.cell_to_entities(i, 1).unwrap())
//             .collect::<Vec<_>>();
//         let e_to_c = (0..g.entity_count(ReferenceCellType::Interval))
//             .map(|i| {
//                 g.entity_to_cells(1, i)
//                     .unwrap()
//                     .iter()
//                     .map(|x| x.cell)
//                     .collect::<Vec<_>>()
//             })
//             .collect::<Vec<_>>();

//         for (i, cell) in c_to_e.iter().enumerate() {
//             for v in *cell {
//                 assert!(e_to_c[*v].contains(&i));
//             }
//         }
//         for (i, cells) in e_to_c.iter().enumerate() {
//             for c in cells {
//                 assert!(c_to_e[*c].contains(&i));
//             }
//         }
//     }

//     #[test]
//     fn test_diameter() {
//         //! Test cell diameters
//         let g = example_grid_flat();

//         for cell_i in 0..2 {
//             assert_relative_eq!(
//                 g.diameter(cell_i),
//                 2.0 * f64::sqrt(1.5 - f64::sqrt(2.0)),
//                 epsilon = 1e-12
//             );
//         }

//         let g = example_grid_3d();

//         for (cell_i, d) in [2.0 / f64::sqrt(6.0), 2.0 * f64::sqrt(1.5 - f64::sqrt(2.0))]
//             .iter()
//             .enumerate()
//         {
//             assert_relative_eq!(g.diameter(cell_i), d, epsilon = 1e-12);
//         }
//     }

//     #[test]
//     fn test_volume() {
//         //! Test cell volumes
//         let g = example_grid_flat();

//         for cell_i in 0..2 {
//             assert_relative_eq!(g.volume(cell_i), 0.5, epsilon = 1e-12);
//         }

//         let g = example_grid_3d();

//         for (cell_i, d) in [f64::sqrt(0.75), 0.5].iter().enumerate() {
//             assert_relative_eq!(g.volume(cell_i), d, epsilon = 1e-12);
//         }
//     }
// }

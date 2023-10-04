//! Implementation of field translations for each FMM.
use std::{
    collections::HashMap,
    hash::Hash,
    ops::{Deref, DerefMut, Mul},
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::Instant,
};

use bempp_tools::Array3D;
use fftw::types::*;
use itertools::Itertools;
use num::Zero;
use num::{Complex, FromPrimitive};
use rayon::prelude::*;

use bempp_field::{
    fft::{irfft3_fftw, irfft3_fftw_par_vec, rfft3_fftw, rfft3_fftw_par_vec},
    types::{FftFieldTranslationKiFmm, SvdFieldTranslationKiFmm},
};

use bempp_traits::{
    arrays::Array3DAccess,
    field::{FieldTranslation, FieldTranslationData},
    fmm::{Fmm, InteractionLists, SourceTranslation, TargetTranslation},
    kernel::{Kernel, KernelScale},
    tree::Tree,
    types::EvalType,
};
use bempp_tree::types::{morton::MortonKey, single_node::SingleNodeTree};
use rlst::{
    common::tools::PrettyPrint,
    common::traits::*,
    dense::{
        global, rlst_col_vec, rlst_mat, rlst_pointer_mat, rlst_rand_col_vec, traits::*, Dot, Shape,
    },
};

use crate::{
    constants::CACHE_SIZE,
    types::{FmmData, KiFmm},
};

type FftMatrixc64 = rlst::dense::Matrix<
    c64,
    rlst::dense::base_matrix::BaseMatrix<c64, rlst::dense::VectorContainer<c64>, Dynamic, Dynamic>,
    Dynamic,
    Dynamic,
>;

impl<T, U> SourceTranslation for FmmData<KiFmm<SingleNodeTree, T, U>>
where
    T: Kernel<T = f64> + KernelScale<T = f64> + std::marker::Send + std::marker::Sync,
    U: FieldTranslationData<T> + std::marker::Sync + std::marker::Send,
{
    /// Point to multipole evaluations, multithreaded over each leaf box.
    fn p2m<'a>(&self) {
        if let Some(leaves) = self.fmm.tree().get_leaves() {
            leaves.par_iter().for_each(move |&leaf| {
                let leaf_multipole_arc = Arc::clone(self.multipoles.get(&leaf).unwrap());
                let fmm_arc = Arc::clone(&self.fmm);

                if let Some(leaf_points) = self.points.get(&leaf) {
                    let leaf_charges_arc = Arc::clone(self.charges.get(&leaf).unwrap());

                    // Lookup data
                    let leaf_coordinates = leaf_points
                        .iter()
                        .map(|p| p.coordinate)
                        .flat_map(|[x, y, z]| vec![x, y, z])
                        .collect_vec();

                    let global_idxs = leaf_points
                        .iter()
                        .map(|p| p.global_idx)
                        .collect_vec();

                    let nsources = leaf_coordinates.len() / self.fmm.kernel.space_dimension();

                    // Get into row major order
                    let leaf_coordinates = unsafe {
                        rlst_pointer_mat!['a, f64, leaf_coordinates.as_ptr(), (nsources, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                    }.eval();

                    let upward_check_surface = leaf.compute_surface(
                        &fmm_arc.tree().domain,
                        fmm_arc.order,
                        fmm_arc.alpha_outer,
                    );
                    let ntargets = upward_check_surface.len() / fmm_arc.kernel.space_dimension();

                    let leaf_charges = leaf_charges_arc.deref();

                    // Calculate check potential
                    let mut check_potential = rlst_col_vec![f64, ntargets];

                    fmm_arc.kernel.evaluate_st(
                        EvalType::Value,
                        leaf_coordinates.data(),
                        &upward_check_surface[..],
                        &leaf_charges[..],
                        check_potential.data_mut(),
                    );

                    let leaf_multipole_owned = (
                        fmm_arc.kernel.scale(leaf.level())
                        * fmm_arc.uc2e_inv.dot(&check_potential)
                    ).eval();

                    let mut leaf_multipole_lock = leaf_multipole_arc.lock().unwrap();

                    *leaf_multipole_lock.deref_mut() = (leaf_multipole_lock.deref() + leaf_multipole_owned).eval();
                }
            });
        }
    }

    /// Multipole to multipole translations, multithreaded over all boxes at a given level.
    fn m2m<'a>(&self, level: u64) {
        // Parallelise over nodes at a given level
        if let Some(sources) = self.fmm.tree().get_keys(level) {
            sources.par_iter().for_each(move |&source| {
                let operator_index = source.siblings().iter().position(|&x| x == source).unwrap();
                let source_multipole_arc = Arc::clone(self.multipoles.get(&source).unwrap());
                let target_multipole_arc =
                    Arc::clone(self.multipoles.get(&source.parent()).unwrap());
                let fmm_arc = Arc::clone(&self.fmm);

                let source_multipole_lock = source_multipole_arc.lock().unwrap();

                let target_multipole_owned =
                    fmm_arc.m2m[operator_index].dot(&source_multipole_lock);

                let mut target_multipole_lock = target_multipole_arc.lock().unwrap();

                *target_multipole_lock.deref_mut() =
                    (target_multipole_lock.deref() + target_multipole_owned).eval();
            })
        }
    }
}

impl<T, U> TargetTranslation for FmmData<KiFmm<SingleNodeTree, T, U>>
where
    T: Kernel<T = f64> + KernelScale<T = f64> + std::marker::Sync + std::marker::Send,
    U: FieldTranslationData<T> + std::marker::Sync + std::marker::Send,
{
    fn l2l(&self, level: u64) {
        if let Some(targets) = self.fmm.tree().get_keys(level) {
            targets.par_iter().for_each(move |&target| {
                let source_local_arc = Arc::clone(self.locals.get(&target.parent()).unwrap());
                let target_local_arc = Arc::clone(self.locals.get(&target).unwrap());
                let fmm = Arc::clone(&self.fmm);

                let operator_index = target.siblings().iter().position(|&x| x == target).unwrap();

                let source_local_lock = source_local_arc.lock().unwrap();

                let target_local_owned = fmm.l2l[operator_index].dot(&source_local_lock);
                let mut target_local_lock = target_local_arc.lock().unwrap();

                *target_local_lock.deref_mut() =
                    (target_local_lock.deref() + target_local_owned).eval();
            })
        }
    }

    fn m2p<'a>(&self) {
        if let Some(targets) = self.fmm.tree().get_leaves() {
            targets.par_iter().for_each(move |&target| {

                let fmm_arc = Arc::clone(&self.fmm);

                if let Some(points) = fmm_arc.tree().get_points(&target) {
                    let target_potential_arc = Arc::clone(self.potentials.get(&target).unwrap());
                    if let Some(w_list) = fmm_arc.get_w_list(&target) {
                        for source in w_list.iter() {
                            let source_multipole_arc =
                                Arc::clone(self.multipoles.get(source).unwrap());

                            let upward_equivalent_surface = source.compute_surface(
                                fmm_arc.tree().get_domain(),
                                fmm_arc.order(),
                                fmm_arc.alpha_inner,
                            );

                            let source_multipole_lock = source_multipole_arc.lock().unwrap();

                            let target_coordinates = points
                                .iter()
                                .map(|p| p.coordinate)
                                .flat_map(|[x, y, z]| vec![x, y, z])
                                .collect_vec();

                            let ntargets = target_coordinates.len() / self.fmm.kernel.space_dimension();

                            // Get into row major order
                            let target_coordinates = unsafe {
                                rlst_pointer_mat!['a, f64, target_coordinates.as_ptr(), (ntargets, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                            }.eval();

                            let mut target_potential = rlst_col_vec![f64, ntargets];

                            fmm_arc.kernel.evaluate_st(
                                EvalType::Value,
                                &upward_equivalent_surface[..],
                                target_coordinates.data(),
                                source_multipole_lock.data(),
                                target_potential.data_mut(),
                            );

                            let mut target_potential_lock = target_potential_arc.lock().unwrap();

                            *target_potential_lock.deref_mut() = (target_potential_lock.deref() + target_potential).eval();
                        }
                    }
                }
            }
)
        }
    }

    fn l2p<'a>(&self) {
        if let Some(targets) = self.fmm.tree().get_leaves() {
            targets.par_iter().for_each(move |&leaf| {
                let fmm_arc = Arc::clone(&self.fmm);
                let source_local_arc = Arc::clone(self.locals.get(&leaf).unwrap());

                if let Some(target_points) = fmm_arc.tree().get_points(&leaf) {
                    let target_potential_arc = Arc::clone(self.potentials.get(&leaf).unwrap());
                    // Lookup data
                    let target_coordinates = target_points
                        .iter()
                        .map(|p| p.coordinate)
                        .flat_map(|[x, y, z]| vec![x, y, z])
                        .collect_vec();
                    let ntargets = target_coordinates.len() / self.fmm.kernel.space_dimension();

                    // Get into row major order
                    let target_coordinates = unsafe {
                        rlst_pointer_mat!['a, f64, target_coordinates.as_ptr(), (ntargets, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                    }.eval();

                    let downward_equivalent_surface = leaf.compute_surface(
                        &fmm_arc.tree().domain,
                        fmm_arc.order,
                        fmm_arc.alpha_outer,
                    );

                    let source_local_lock = source_local_arc.lock().unwrap();

                    let mut target_potential = rlst_col_vec![f64, ntargets];

                    fmm_arc.kernel.evaluate_st(
                        EvalType::Value,
                        &downward_equivalent_surface[..],
                        target_coordinates.data(),
                        source_local_lock.data(),
                        target_potential.data_mut(),
                    );

                    let mut target_potential_lock = target_potential_arc.lock().unwrap();

                    *target_potential_lock.deref_mut() = (target_potential_lock.deref() + target_potential).eval();
                }
            })
        }
    }

    fn p2l<'a>(&self) {
        if let Some(targets) = self.fmm.tree().get_leaves() {
            targets.par_iter().for_each(move |&leaf| {
                let fmm_arc = Arc::clone(&self.fmm);
                let target_local_arc = Arc::clone(self.locals.get(&leaf).unwrap());

                if let Some(x_list) = fmm_arc.get_x_list(&leaf) {
                    for source in x_list.iter() {
                        if let Some(source_points) = fmm_arc.tree().get_points(source) {
                            let source_coordinates = source_points
                                .iter()
                                .map(|p| p.coordinate)
                                .flat_map(|[x, y, z]| vec![x, y, z])
                                .collect_vec();

                            let nsources = source_coordinates.len() / self.fmm.kernel.space_dimension();

                            // Get into row major order
                            let source_coordinates = unsafe {
                                rlst_pointer_mat!['a, f64, source_coordinates.as_ptr(), (nsources, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                            }.eval();

                            let source_charges = self.charges.get(source).unwrap();

                            let downward_check_surface = leaf.compute_surface(
                                &fmm_arc.tree().domain,
                                fmm_arc.order,
                                fmm_arc.alpha_inner,
                            );

                            let ntargets = downward_check_surface.len() / fmm_arc.kernel.space_dimension();
                            let mut downward_check_potential = rlst_col_vec![f64, ntargets];

                            fmm_arc.kernel.evaluate_st(
                                EvalType::Value,
                                source_coordinates.data(),
                                &downward_check_surface[..],
                                &source_charges[..],
                                downward_check_potential.data_mut()
                            );


                            let mut target_local_lock = target_local_arc.lock().unwrap();

                            let target_local_owned = (fmm_arc.kernel.scale(leaf.level()) * fmm_arc.dc2e_inv.dot(&downward_check_potential)).eval();

                            *target_local_lock.deref_mut() = (target_local_lock.deref() + target_local_owned).eval();
                        }
                    }
                }
            })
        }
    }

    fn p2p<'a>(&self) {
        if let Some(targets) = self.fmm.tree().get_leaves() {
            targets.par_iter().for_each(move |&target| {
                let fmm_arc = Arc::clone(&self.fmm);

                if let Some(target_points) = fmm_arc.tree().get_points(&target) {
                    let target_potential_arc = Arc::clone(self.potentials.get(&target).unwrap());
                    let target_coordinates = target_points
                        .iter()
                        .map(|p| p.coordinate)
                        .flat_map(|[x, y, z]| vec![x, y, z])
                        .collect_vec();

                    let ntargets= target_coordinates.len() / self.fmm.kernel.space_dimension();

                    // Get into row major order
                    let target_coordinates = unsafe {
                        rlst_pointer_mat!['a, f64, target_coordinates.as_ptr(), (ntargets, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                    }.eval();

                    if let Some(u_list) = fmm_arc.get_u_list(&target) {
                        for source in u_list.iter() {
                            if let Some(source_points) = fmm_arc.tree().get_points(source) {
                                let source_coordinates = source_points
                                    .iter()
                                    .map(|p| p.coordinate)
                                    .flat_map(|[x, y, z]| vec![x, y, z])
                                    .collect_vec();

                                let nsources = source_coordinates.len() / self.fmm.kernel.space_dimension();

                                // Get into row major order
                                let source_coordinates = unsafe {
                                    rlst_pointer_mat!['a, f64, source_coordinates.as_ptr(), (nsources, fmm_arc.kernel.space_dimension()), (fmm_arc.kernel.space_dimension(), 1)]
                                }.eval();

                                let source_charges_arc =
                                    Arc::clone(self.charges.get(source).unwrap());

                                let mut target_potential = rlst_col_vec![f64, ntargets];

                                fmm_arc.kernel.evaluate_st(
                                    EvalType::Value,
                                    source_coordinates.data(),
                                    target_coordinates.data(),
                                    &source_charges_arc[..],
                                    target_potential.data_mut(),
                                );

                                let mut target_potential_lock =
                                    target_potential_arc.lock().unwrap();

                                *target_potential_lock.deref_mut() = (target_potential_lock.deref() + target_potential).eval();
                            }
                        }
                    }
                }
            })
        }
    }
}

/// Implement the multipole to local translation operator for an SVD accelerated KiFMM on a single node.
impl<T> FieldTranslation for FmmData<KiFmm<SingleNodeTree, T, SvdFieldTranslationKiFmm<T>>>
where
    T: Kernel<T = f64> + KernelScale<T = f64> + std::marker::Sync + std::marker::Send + Default,
{
    fn m2l<'a>(&self, level: u64) {
        let Some(targets) = self.fmm.tree().get_keys(level) else { return };
        let mut transfer_vector_to_m2l =
            HashMap::<usize, Arc<Mutex<Vec<(MortonKey, MortonKey)>>>>::new();

        for tv in self.fmm.m2l.transfer_vectors.iter() {
            transfer_vector_to_m2l.insert(tv.hash, Arc::new(Mutex::new(Vec::new())));
        }

        let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);

        targets.par_iter().enumerate().for_each(|(_i, &target)| {
            if let Some(v_list) = self.fmm.get_v_list(&target) {
                let calculated_transfer_vectors = v_list
                    .iter()
                    .map(|source| target.find_transfer_vector(source))
                    .collect::<Vec<usize>>();
                for (transfer_vector, &source) in
                    calculated_transfer_vectors.iter().zip(v_list.iter())
                {
                    let m2l_arc = Arc::clone(transfer_vector_to_m2l.get(transfer_vector).unwrap());
                    let mut m2l_lock = m2l_arc.lock().unwrap();
                    m2l_lock.push((source, target));
                }
            }
        });

        let mut transfer_vector_to_m2l_rw_lock =
            HashMap::<usize, Arc<RwLock<Vec<(MortonKey, MortonKey)>>>>::new();

        // Find all multipole expansions and allocate
        for (&transfer_vector, m2l_arc) in transfer_vector_to_m2l.iter() {
            transfer_vector_to_m2l_rw_lock.insert(
                transfer_vector,
                Arc::new(RwLock::new(m2l_arc.lock().unwrap().clone())),
            );
        }

        transfer_vector_to_m2l_rw_lock
            .par_iter()
            .for_each(|(transfer_vector, m2l_arc)| {
                let c_idx = self
                    .fmm
                    .m2l
                    .transfer_vectors
                    .iter()
                    .position(|x| x.hash == *transfer_vector)
                    .unwrap();

                let (nrows, _) = self.fmm.m2l.m2l.c.shape();
                let top_left = (0, c_idx * self.fmm.m2l.k);
                let dim = (nrows, self.fmm.m2l.k);

                let c_sub = self.fmm.m2l.m2l.c.block(top_left, dim);

                let m2l_rw = m2l_arc.read().unwrap();
                let mut multipoles = rlst_mat![f64, (self.fmm.m2l.k, m2l_rw.len())];

                for (i, (source, _)) in m2l_rw.iter().enumerate() {
                    let source_multipole_arc = Arc::clone(self.multipoles.get(source).unwrap());
                    let source_multipole_lock = source_multipole_arc.lock().unwrap();

                    // Compressed multipole
                    let compressed_source_multipole_owned =
                        self.fmm.m2l.m2l.st_block.dot(&source_multipole_lock).eval();

                    let first = i * self.fmm.m2l.k;
                    let last = first + self.fmm.m2l.k;

                    let multipole_slice = multipoles.get_slice_mut(first, last);
                    multipole_slice.copy_from_slice(compressed_source_multipole_owned.data());
                }

                // Compute convolution
                let compressed_check_potential_owned = c_sub.dot(&multipoles);

                // Post process to find check potential
                let check_potential_owned = self
                    .fmm
                    .m2l
                    .m2l
                    .u
                    .dot(&compressed_check_potential_owned)
                    .eval();

                // Compute local
                let locals_owned = (self.fmm.dc2e_inv.dot(&check_potential_owned)
                    * self.fmm.kernel.scale(level)
                    * self.m2l_scale(level))
                .eval();

                // Assign locals
                for (i, (_, target)) in m2l_rw.iter().enumerate() {
                    let target_local_arc = Arc::clone(self.locals.get(target).unwrap());
                    let mut target_local_lock = target_local_arc.lock().unwrap();

                    let top_left = (0, i);
                    let dim = (ncoeffs, 1);
                    let target_local_owned = locals_owned.block(top_left, dim);

                    *target_local_lock.deref_mut() =
                        (target_local_lock.deref() + target_local_owned).eval();
                }
            });
    }

    fn m2l_scale(&self, level: u64) -> f64 {
        if level < 2 {
            panic!("M2L only performed on level 2 and below")
        }

        if level == 2 {
            1. / 2.
        } else {
            2_f64.powf((level - 3) as f64)
        }
    }
}

/// Implement the multipole to local translation operator for an FFT accelerated KiFMM on a single node.
impl<T> FieldTranslation for FmmData<KiFmm<SingleNodeTree, T, FftFieldTranslationKiFmm<T>>>
where
    T: Kernel<T = f64> + KernelScale<T = f64> + std::marker::Sync + std::marker::Send + Default,
{
    fn m2l<'a>(&self, level: u64) {
        assert!(level >= 2);
        let (Some(targets), Some(parents) )= (self.fmm.tree().get_keys(level), self.fmm.tree().get_keys(level-1))else { return };

        // Form signals to use for convolution first
        let start = Instant::now();

        // let n = 2*self.fmm.order -1;
        // let mut padded_signals: DashMap<MortonKey, Array3D<f64>> = targets.iter().map(|target| (*target, Array3D::<f64>::new((n, n, n)))).collect();
        // let mut padded_signals_hat: DashMap<MortonKey, Array3D<c64>> = targets.iter().map(|target| (*target, Array3D::<c64>::new((n, n, n/2 + 1)))).collect();

        // // Pad the signal
        // let &(m, n, o) = &(n, n, n);

        // let p = m + 1;
        // let q = n + 1;
        // let r = o + 1;

        // let pad_size = (p-m, q-n, r-o);
        // let pad_index = (p-m, q-n, r-o);
        // let real_dim = q;

        // targets.par_iter().for_each(|target| {
        //     let fmm_arc = Arc::clone(&self.fmm);
        //     let source_multipole_arc = Arc::clone(self.multipoles.get(target).unwrap());
        //     let source_multipole_lock = source_multipole_arc.lock().unwrap();
        //     let signal = fmm_arc.m2l.compute_signal(fmm_arc.order, source_multipole_lock.data());

        //     let mut padded_signal = pad3(&signal, pad_size, pad_index);
        //     padded_signals_hat.insert(*target, Array3D::<c64>::new((p, q, r/2 + 1)));
        //     padded_signals.insert(*target, padded_signal);
        // });
        // Compute FFT of signals for use in convolution
        // let ntargets = targets.len();
        // let key = targets[0];
        // let shape = padded_signals.get(&key).unwrap().shape().clone();
        // let shape = [shape.0, shape.1, shape.2];

        // let start = Instant::now();
        // rfft3_fftw_par_dm(&padded_signals, &padded_signals_hat, &shape, targets);
        // println!("FFT time {:?}", start.elapsed().as_millis());
        // Loop through padded signals and apply convolutions in all directions of transfer vector, even if there are zeros.

        // (0..self.fmm.m2l.transfer_vectors.len()).into_par_iter().for_each(|k_idx| {
        //     let fmm_arc = Arc::clone(&self.fmm);
        //     // let padded_kernel_hat = &fmm_arc.m2l.m2l[k_idx];
        //     // let &(m_, n_, o_) = padded_kernel_hat.shape();
        //     // let len_padded_kernel_hat= m_*n_*o_;

        //     // let padded_kernel_hat= unsafe {
        //     //     rlst_pointer_mat!['a, Complex<f64>, padded_kernel_hat.get_data().as_ptr(), (len_padded_kernel_hat, 1), (1,1)]
        //     // };

        // });

        // padded_signals_hat.par_iter().for_each(|pair| {
        //     let source = pair.key();
        //     let padded_signal_hat = pair.value();
        //     let fmm_arc = Arc::clone(&self.fmm);

        //     // Compute Hadamard product
        //     let &(m_, n_, o_) = padded_signal_hat.shape();
        //     let len_padded_signal_hat= m_*n_*o_;
        //     let padded_signal_hat = unsafe {
        //         rlst_pointer_mat!['a, Complex<f64>, padded_signal_hat.get_data().as_ptr(), (len_padded_signal_hat, 1), (1,1)]
        //     };

        //     for (k_idx, tv) in self.fmm.m2l.transfer_vectors.iter().enumerate() {
        //             let padded_kernel_hat = &fmm_arc.m2l.m2l[k_idx];
        //             let &(m_, n_, o_) = padded_kernel_hat.shape();
        //             let len_padded_kernel_hat= m_*n_*o_;

        //             let padded_kernel_hat= unsafe {
        //                 rlst_pointer_mat!['a, Complex<f64>, padded_kernel_hat.get_data().as_ptr(), (len_padded_kernel_hat, 1), (1,1)]
        //             };

        //             let mut check_potential_hat = padded_kernel_hat.cmp_wise_product(&padded_signal_hat).eval();

        //     }
        // })

        //////////////////////////////////
        // let n = 2 * self.fmm.order - 1;
        // let ntargets = targets.len();

        // // Pad the signal
        // let &(m, n, o) = &(n, n, n);

        // let p = m + 1;
        // let q = n + 1;
        // let r = o + 1;
        // let size = p * q * r;
        // let size_real = p * q * (r / 2 + 1);
        // let pad_size = (p - m, q - n, r - o);
        // let pad_index = (p - m, q - n, r - o);
        // let real_dim = q;

        // // Pad all multipole coefficients at this level ready for FFT
        // let mut padded_signals = rlst_mat![f64, (size * ntargets, 1)];

        // padded_signals
        //     .data_mut()
        //     .par_chunks_mut(size)
        //     .zip((0..ntargets).into_par_iter())
        //     .for_each(|(chunk, i)| {
        //         let fmm_arc = Arc::clone(&self.fmm);
        //         let target = &targets[i];
        //         let source_multipole_arc = Arc::clone(self.multipoles.get(&target).unwrap());
        //         let source_multipole_lock = source_multipole_arc.lock().unwrap();
        //         let signal = fmm_arc
        //             .m2l
        //             .compute_signal(fmm_arc.order, source_multipole_lock.data());

        //         let padded_signal = pad3(&signal, pad_size, pad_index);

        //         chunk.copy_from_slice(padded_signal.get_data());
        //     });

        // let mut padded_signals_hat = rlst_mat![c64, (size_real * ntargets, 1)]; // equivalent to fft_in
        // let mut check_potential_hat = rlst_mat![c64, (size_real * ntargets, 1)]; // equivalent to fft_out

        // println!("data organisation time {:?}", start.elapsed().as_millis());

        // // // Each index maps to a target (sorted) from targets
        // // let mut padded_signals_hat = vec![Arc::new(Mutex::new(vec![Complex::<f64>::zero(); size_real])); ntargets];

        // let start = Instant::now();
        // rfft3_fftw_par_vec(&mut padded_signals, &mut padded_signals_hat, &[p, q, r]);
        // // rfft3_fftw_par_vec_arc_mutex(&mut padded_signals, &mut padded_signals_hat, &[p, q, r]);

        // // I should form fft_in and fft_out here, where sibling sets are grouped, and re-organised in terms
        // // of frequency

        // padded_signals_hat
        //     .data_mut()
        //     .par_chunks_mut(8 * size_real)
        //     .for_each(|sibling_chunk| {
        //         let mut buffer = rlst_mat![c64, (8 * size_real, 1)];

        //         // Fill up buffer
        //         buffer
        //             .data_mut()
        //             .iter_mut()
        //             .zip(sibling_chunk.iter())
        //             .for_each(|(b, s)| *b = *s);

        //         // Reorganise by frequency
        //         for i in 0..8 {
        //             for j in 0..size_real {
        //                 sibling_chunk[j * 8 + i] = buffer.data()[i * size_real + j];
        //             }
        //         }
        //     });

        // println!("fft time {:?}", start.elapsed().as_millis());

        // let start = Instant::now();
        // // For each target, I have to find the convolutions associated with its V list
        // // Begin by associating each target with a local index

        // let mut parent_index_pointer = HashMap::new();
        // parents.iter().enumerate().for_each(|(i, parent)| {
        //     parent_index_pointer.insert(parent, i);
        // });

        // // Now I can iterate through the targets and find the index pointers associated with its sources
        // let nblk_trg = parents.len() * std::mem::size_of::<f64>() / CACHE_SIZE;
        // let nblk_trg = std::cmp::max(1, nblk_trg); // minimum of 1 block

        // let mut interaction_offsets_f = Vec::new();
        // let mut interaction_count = Vec::new();
        // let mut interaction_count_offset_: usize = 0;

        // for iblk_trg in 0..nblk_trg {
        //     let blk_start = (parents.len() * iblk_trg) / nblk_trg;
        //     let blk_end = (parents.len() * (iblk_trg + 1)) / nblk_trg;
        //     for ipos in 0..26 {
        //         for i in blk_start..blk_end {
        //             let parent = &parents[i];
        //             let v_list = parent.all_neighbors();
        //             if let Some(key) = &v_list[ipos] {
        //                 interaction_offsets_f.push(*parent_index_pointer.get(key).unwrap());
        //                 interaction_offsets_f.push(i);
        //                 interaction_count_offset_ += 1;
        //             }
        //         }

        //         interaction_count.push(interaction_count_offset_);
        //     }
        // }

        // // Organising blocks by source/target pairs that share an M2L interaction
        // let nblk_inter = interaction_count.len();

        // // Outer slice index are parents, and elements are pointers to the first child's k'th frequency
        // let mut in_: Vec<Vec<Option<&mut Complex<f64>>>> = Vec::new();
        // for _ in 0..parents.len() {
        //     in_.push(Vec::new())
        // }

        // padded_signals_hat
        //     .data_mut()
        //     .chunks_exact_mut(8 * size_real)
        //     .zip(in_.iter_mut())
        //     .for_each(|(padded_signal_hat_chunk, in_chunk)| {
        //         // Calculate mutable pointers to all frequency chunks
        //         let mut remaining = padded_signal_hat_chunk;
        //         while remaining.len() >= 8 {
        //             let (front, rest) = remaining.split_at_mut(8);

        //             // let raw: *mut Complex<f64> = &mut front[0];

        //             in_chunk.push(Some(&mut front[0]));
        //             remaining = rest;
        //         }
        //     });

        // println!("HERE {:?} {:?}", in_[0].len(), in_.len());

        // (0..nblk_inter).into_par_iter().for_each(|iblk_inter| {

        //     let interaction_count_offset0;
        //     let interaction_count_offset1 = interaction_count[iblk_inter];
        //     if iblk_inter == 0 {
        //         interaction_count_offset0 = 0
        //     } else {
        //         interaction_count_offset0 = interaction_count[iblk_inter-1]
        //     }
        //     let interaction_count = interaction_count_offset1 - interaction_count_offset0;

        //     // For each interaction
        //     for j in 0..interaction_count {

        //         // Find associated source and target pointer
        //         let source_index_pointer = interaction_offsets_f[interaction_count_offset0+j];
        //         let target_index_pointer = interaction_offsets_f[interaction_count_offset0+j+1];

        //         let (_, mut source_ptr) = padded_signals_hat.data().split_at(source_index_pointer);

        //         // // For each frequency chunk need a mutable pointer for this interaction
        //         // for k in 0..size_real {

        //         //     let source_lidx = source_index_pointer*size_real+k*8;
        //         //     let source_ridx = source_lidx+8;

        //         //     let target_lidx = target_index_pointer*size_real+k*8;
        //         //     let target_ridx = target_lidx+8;

        //         // }
        //     }
        // });

        println!("index pointer time {:?}", start.elapsed().as_millis());

        // // Can now compute Hadamard product
        // // let nblk_trg = nblk_inter / 26;

        // (0..nblk_trg).into_iter().for_each(|iblk_trg| {
        //     (0..size_real).into_par_iter().for_each(|k| {
        //         (0..26).into_iter().for_each(|ipos| {

        //             let iblk_inter = iblk_trg*26+ipos; // gets number of interactions for each ipos, for this iblk

        //             let interaction_count_offset0;
        //             let interaction_count_offset1 = interaction_count[iblk_inter];
        //             if iblk_inter == 0 {
        //                 interaction_count_offset0 = 0
        //             } else {
        //                 interaction_count_offset0 = interaction_count[iblk_inter-1]
        //             }
        //             let interaction_count = interaction_count_offset1 - interaction_count_offset0;
        //             // Can now load into memory associated Kernel Matrix at the K'th frequency
        //             // let M = &self.fmm.m2l.m2l[ipos];

        //             // Need access to mutable pointer corresponding to k'th frequency of the sources
        //             // And same thing to k'th frequency of the targets

        //             // println!("Interaction Count  {:?} for ipos {:?} at iblk_trg {:?}", interaction_count, ipos, iblk_trg);

        //         })
        //     })
        // })

        // // Map between keys and index locations in targets at this level
        // let mut target_index_map = Arc::new(RwLock::new(HashMap::new()));

        // for (i, target) in targets.iter().enumerate() {

        //     let mut map = target_index_map.write().unwrap();
        //     map.insert(*target, i);
        // }

        // // Each index corresponds to a target, and contains a vector of pointers to the padded signals in the targets interactions list
        // let mut source_index_pointer: Vec<Arc<Mutex<Vec<Arc<Mutex<Vec<Complex<f64>>>>>>>> =
        //     (0..ntargets).map(|_| Arc::new(Mutex::new(Vec::<Arc<Mutex<Vec<Complex<f64>>>>>::new()))).collect();

        // targets
        //     .into_par_iter()
        //     .zip(source_index_pointer.par_iter_mut())
        //     .enumerate()
        //     .for_each(|(i, (target, arc_mutex_vec))| {

        //         let fmm_arc = Arc::clone(&self.fmm);
        //         let v_list = target
        //             .parent()
        //             .neighbors()
        //             .iter()
        //             .flat_map(|pn| pn.children())
        //             .filter(|pnc| !target.is_adjacent_same_level(pnc))
        //             .collect_vec();

        //         // Lookup indices for each element of v_list and add the pointers to the underlying data to the index pointer
        //         let mut indices = Vec::new();
        //         let target_index_map_arc = Arc::clone(&target_index_map);
        //         let map = target_index_map.read().unwrap();
        //         for source in v_list.iter() {
        //             let idx = map.get(source).unwrap();
        //             indices.push(*idx);
        //         }

        //         let mut outer_vec: MutexGuard<'_, Vec<Arc<Mutex<Vec<Complex<f64>>>>>> = arc_mutex_vec.lock().unwrap();
        //         for &idx in indices.iter() {
        //             let tmp: Arc<Mutex<Vec<Complex<f64>>>> = Arc::clone(&padded_signals_hat[idx]);
        //             outer_vec.push(tmp);
        //         }
        // });

        // // Compute Hadamard product with elements of V List, now stored in source_index_pointer

        // let start = Instant::now();
        // // let mut global_check_potentials_hat = vec![Arc::new(Mutex::new(vec![Complex::<f64>::zero(); size_real])); ntargets];

        // let mut global_check_potentials_hat = (0..ntargets)
        //     .map(|_| Arc::new(Mutex::new(vec![Complex::<f32>::zero(); size_real]))).collect_vec();
        // // let mut global_check_potentials_hat = (0..ntargets)
        // //     .map(|_| Arc::new(Mutex::new(vec![0f64; size_real]))).collect_vec();

        // global_check_potentials_hat
        //     .par_iter_mut()
        //     .zip(
        //         source_index_pointer
        //         .into_par_iter()
        //     )
        //     .zip(
        //         targets.into_par_iter()
        //     ).for_each(|((check_potential_hat, sources), target)| {

        //         // Find the corresponding Kernel matrices for each signal
        //         let fmm_arc = Arc::clone(&self.fmm);
        //         let v_list = target
        //             .parent()
        //             .neighbors()
        //             .iter()
        //             .flat_map(|pn| pn.children())
        //             .filter(|pnc| !target.is_adjacent_same_level(pnc))
        //             .collect_vec();

        //         let k_idxs = v_list
        //             .iter()
        //             .map(|source| target.find_transfer_vector(source))
        //             .map(|tv| {
        //                 fmm_arc
        //                 .m2l
        //                 .transfer_vectors
        //                 .iter()
        //                 .position(|x| x.vector == tv)
        //                 .unwrap()
        //             }).collect_vec();

        //         // Compute convolutions
        //         let check_potential_hat_arc = Arc::clone(check_potential_hat);
        //         let mut check_potential_hat_data = check_potential_hat_arc.lock().unwrap();

        //         let tmp = sources.lock().unwrap();
        //         let mut result = vec![Complex::<f64>::zero(); size_real];

        //         // for i in 0..result.len() {
        //         //     for _ in 0..189 {
        //         //         result[i] += Complex::<f64>::from(1.0);
        //         //     }
        //         // }

        //         for i in 0..1 {

        //             let psh = tmp[i].lock().unwrap();
        //             let pkh = &fmm_arc.m2l.m2l[k_idxs[i]].get_data();

        //             let hadamard: Vec<c64> = psh.iter().zip(pkh.iter()).map(|(s, k)| {*s * *k}).collect_vec();
        //             for j in 0..result.len() {
        //                 result[j] += Complex::<f64>::from(1.0);
        //             }
        //         }

        // for ((i, source), &k_idx) in tmp.iter().enumerate().zip(k_idxs.iter()) {

        //     // let psh = source.lock().unwrap();
        //     // let pkh = &fmm_arc.m2l.m2l[k_idx];

        //     // let psh = unsafe {
        //     //     rlst_pointer_mat!['a, c64, psh.as_ptr(), (size_real, 1), (1,1)]
        //     // };

        //     // let pkh = unsafe {
        //     //     rlst_pointer_mat!['a, c64, pkh.get_data().as_ptr(), (size_real, 1), (1,1)]
        //     // };

        //     // let hadamard = psh.cmp_wise_product(&pkh).eval();
        //     // result.iter_mut().zip(hadamard.data().iter()).for_each(|(r, h)| *r += h);

        //     let psh = source.lock().unwrap();
        //     let pkh = &fmm_arc.m2l.m2l[k_idx].get_data();

        //     let hadamard: Vec<c64> = psh.iter().zip(pkh.iter()).map(|(s, k)| {*s * *k}).collect_vec();

        //     for j in 0..result.len() {
        //         result[j] += Complex::<f64>::from(1.0);
        //     }

        //     // result.iter_mut().zip(hadamard.iter()).for_each(|(r, h)| *r += Complex::<f32>::zero())
        //     // result.iter_mut().for_each(|(r)| *r += Complex::<f32>::zero())
        //     // check_potential_hat_data.deref_mut().iter_mut()
        //     //     .zip(hadamard.iter())
        //     //     .for_each(|(r, h)| *r += h);
        //     // check_potential_hat_data.deref_mut().iter_mut()
        //     //     // .zip(hadamard.iter())
        //     //     .for_each(|(r)| *r += Complex::<f64>::from(1.0));

        // }

        // check_potential_hat_data.deref_mut().iter_mut().for_each(|x| *x += Complex::zero());

        // });

        // println!("Hadamard time {:?}", start.elapsed().as_millis());
        // let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);
        // // Compute hadamard product with kernels
        // let range = (0..self.fmm.m2l.transfer_vectors.len()).into_par_iter();
        // self.fmm.m2l.transfer_vectors.iter().take(16).par_bridge().for_each(|tv| {
        //     // Locate correct precomputed FFT of kernel
        //     let k_idx = self.fmm
        //         .m2l
        //         .transfer_vectors
        //         .iter()
        //         .position(|x| x.vector == tv.vector)
        //         .unwrap();
        //     let padded_kernel_hat = &self.fmm.m2l.m2l[k_idx];
        //     let &(m_, n_, o_) = padded_kernel_hat.shape();
        //     let len_padded_kernel_hat= m_*n_*o_;
        //     let padded_kernel_hat= unsafe {
        //         rlst_pointer_mat!['a, Complex<f64>, padded_kernel_hat.get_data().as_ptr(), (len_padded_kernel_hat, 1), (1,1)]
        //     };

        //     let padded_kernel_hat_arc = Arc::new(padded_kernel_hat);

        //     padded_signals_hat.data().chunks_exact(len_padded_kernel_hat).enumerate().for_each(|(i, padded_signal_hat)| {
        //         let padded_signal_hat = unsafe {
        //             rlst_pointer_mat!['a, Complex<f64>, padded_signal_hat.as_ptr(), (len_padded_kernel_hat, 1), (1,1)]
        //         };

        //         let padded_kernel_hat_ref = Arc::clone(&padded_kernel_hat_arc);

        //         let check_potential = padded_signal_hat.cmp_wise_product(padded_kernel_hat_ref.deref()).eval();
        //     });
        // });

        //////////////////////////
        // targets.iter().for_each(move |&target| {
        //     if let Some(v_list) = self.fmm.get_v_list(&target) {
        //         let fmm_arc = Arc::clone(&self.fmm);

        //         let target_local_arc = Arc::clone(self.locals.get(&target).unwrap());

        //         for source in v_list.iter() {

        //             let transfer_vector = target.find_transfer_vector(source);

        //             // Locate correct precomputed FFT of kernel
        //             let k_idx = fmm_arc
        //                 .m2l
        //                 .transfer_vectors
        //                 .iter()
        //                 .position(|x| x.vector == transfer_vector)
        //                 .unwrap();

        //             // Compute FFT of signal
        //             let source_multipole_arc = Arc::clone(self.multipoles.get(source).unwrap());

        //             let source_multipole_lock = source_multipole_arc.lock().unwrap();

        //             // TODO: SLOW ~ 1.5s
        //             let signal = fmm_arc.m2l.compute_signal(fmm_arc.order, source_multipole_lock.data());

        //             // 1. Pad the signal
        //             let &(m, n, o) = signal.shape();

        //             // let p = 2_f64.powf((m as f64).log2().ceil()) as usize;
        //             // let q = 2_f64.powf((n as f64).log2().ceil()) as usize;
        //             // let r = 2_f64.powf((o as f64).log2().ceil()) as usize;
        //             // let p = p.max(4);
        //             // let q = q.max(4);
        //             // let r = r.max(4);

        //             let p = m + 1;
        //             let q = n + 1;
        //             let r = o + 1;

        //             let pad_size = (p-m, q-n, r-o);
        //             let pad_index = (p-m, q-n, r-o);
        //             let real_dim = q;

        //             // Also slow but not as slow as compute signal ~100ms
        //             let mut padded_signal = pad3(&signal, pad_size, pad_index);

        //             // TODO: Very SLOW ~21s
        //             // let padded_signal_hat = rfft3(&padded_signal);
        //             let mut padded_signal_hat = Array3D::<c64>::new((p, q, r/2 + 1));
        //             rfft3_fftw(padded_signal.get_data_mut(), padded_signal_hat.get_data_mut(), &[p, q, r]);
        //             let &(m_, n_, o_) = padded_signal_hat.shape();
        //             let len_padded_signal_hat = m_*n_*o_;

        //             // 2. Compute the convolution to find the check potential
        //             let padded_kernel_hat = &fmm_arc.m2l.m2l[k_idx];
        //             let &(m_, n_, o_) = padded_kernel_hat.shape();
        //             let len_padded_kernel_hat= m_*n_*o_;

        //             // Compute Hadamard product
        //             let padded_signal_hat = unsafe {
        //                 rlst_pointer_mat!['a, Complex<f64>, padded_signal_hat.get_data().as_ptr(), (len_padded_signal_hat, 1), (1,1)]
        //             };

        //             let padded_kernel_hat= unsafe {
        //                 rlst_pointer_mat!['a, Complex<f64>, padded_kernel_hat.get_data().as_ptr(), (len_padded_kernel_hat, 1), (1,1)]
        //             };

        //             let mut check_potential_hat = padded_kernel_hat.cmp_wise_product(padded_signal_hat).eval();

        //             // 3.1 Compute iFFT to find check potentials
        //             let mut check_potential = Array3D::<f64>::new((p, q, r));
        //             irfft3_fftw(check_potential_hat.data_mut(), check_potential.get_data_mut(), &[p, q, r]);

        //             // Filter check potentials
        //             let mut filtered_check_potentials: Array3D<f64> = Array3D::new((m+1, n+1, o+1));
        //             for i in (p-m-1)..p {
        //                 for j in (q-n-1)..q {
        //                     for k in (r-o-1)..r {
        //                         let i_= i - (p-m-1);
        //                         let j_ = j - (q-n-1);
        //                         let k_ = k - (r-o-1);
        //                         *filtered_check_potentials.get_mut(i_, j_, k_).unwrap()= *check_potential.get(i, j, k).unwrap();
        //                     }
        //                 }
        //             }

        //             let (_, target_surface_idxs) = target.surface_grid(fmm_arc.order);
        //             let mut tmp = Vec::new();
        //             let ntargets = target_surface_idxs.len() / fmm_arc.kernel.space_dimension();
        //             let xs = &target_surface_idxs[0..ntargets];
        //             let ys = &target_surface_idxs[ntargets..2*ntargets];
        //             let zs = &target_surface_idxs[2*ntargets..];

        //             for i in 0..ntargets {
        //                 let val = filtered_check_potentials.get(xs[i], ys[i], zs[i]).unwrap();
        //                 tmp.push(*val);
        //             }

        //             let check_potential = unsafe {
        //                 rlst_pointer_mat!['a, f64, tmp.as_ptr(), (ntargets, 1), (1,1)]
        //             };

        //             // Finally, compute local coefficients from check potential
        //             let target_local_owned = (self.m2l_scale(target.level())
        //                 * fmm_arc.kernel.scale(target.level())
        //                 * fmm_arc.dc2e_inv.dot(&check_potential)).eval();

        //             let mut target_local_lock = target_local_arc.lock().unwrap();
        //             *target_local_lock.deref_mut() = (target_local_lock.deref() + target_local_owned).eval();
        //         }
        //     }
        // })
    }

    fn m2l_scale(&self, level: u64) -> f64 {
        if level < 2 {
            panic!("M2L only performed on level 2 and below")
        }
        if level == 2 {
            1. / 2.
        } else {
            2_f64.powf((level - 3) as f64)
        }
    }
}

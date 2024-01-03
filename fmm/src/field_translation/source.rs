//! kiFMM based on simple linear data structures that minimises memory allocations, maximises cache re-use.
use std::collections::HashSet;

use itertools::Itertools;
use num::Float;
use rayon::prelude::*;

use bempp_traits::{
    field::FieldTranslationData,
    fmm::{Fmm, SourceTranslation},
    kernel::{Kernel, ScaleInvariantKernel},
    tree::Tree,
    types::EvalType,
};
use bempp_tree::types::{morton::MortonKey, single_node::SingleNodeTree};

use crate::{
    constants::{M2M_MAX_CHUNK_SIZE, P2M_MAX_CHUNK_SIZE},
    helpers::find_chunk_size,
    types::{FmmDataAdaptive, FmmDataUniform, KiFmmLinear},
};
use rlst::{
    common::traits::*,
    dense::{rlst_col_vec, rlst_pointer_mat, traits::*, Dot, MultiplyAdd, VectorContainer},
};

impl<T, U, V> SourceTranslation for FmmDataUniform<KiFmmLinear<SingleNodeTree<V>, T, U, V>, V>
where
    T: Kernel<T = V> + ScaleInvariantKernel<T = V> + std::marker::Send + std::marker::Sync,
    U: FieldTranslationData<T> + std::marker::Sync + std::marker::Send,
    V: Scalar<Real = V> + Float + Default + std::marker::Sync + std::marker::Send,
    V: MultiplyAdd<
        V,
        VectorContainer<V>,
        VectorContainer<V>,
        VectorContainer<V>,
        Dynamic,
        Dynamic,
        Dynamic,
    >,
{
    /// Point to multipole evaluations, multithreaded over each leaf box.
    fn p2m<'a>(&self) {
        if let Some(leaves) = self.fmm.tree().get_all_leaves() {
            let nleaves = leaves.len();
            let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);

            let surface_size = ncoeffs * self.fmm.kernel.space_dimension();

            let mut check_potentials = rlst_col_vec![V, nleaves * ncoeffs];
            let coordinates = self.fmm.tree().get_all_coordinates().unwrap();
            let dim = self.fmm.kernel.space_dimension();

            // 1. Compute the check potential for each box
            check_potentials
                .data_mut()
                .par_chunks_exact_mut(ncoeffs)
                .zip(self.leaf_upward_surfaces.par_chunks_exact(surface_size))
                .zip(&self.charge_index_pointer)
                .for_each(
                    |((check_potential, upward_check_surface), charge_index_pointer)| {
                        let charges = &self.charges[charge_index_pointer.0..charge_index_pointer.1];
                        let coordinates = &coordinates
                            [charge_index_pointer.0 * dim..charge_index_pointer.1 * dim];

                        let nsources = coordinates.len() / dim;

                        if nsources > 0 {
                            let coordinates = unsafe {
                                rlst_pointer_mat!['a, V, coordinates.as_ptr(), (nsources, dim), (dim, 1)]
                            }.eval();

                            self.fmm.kernel.evaluate_st(
                                EvalType::Value,
                                coordinates.data(),
                                upward_check_surface,
                                charges,
                                check_potential,
                            );
                        }
                    },
                );

            // 2. Compute the multipole expansions, with each of chunk_size boxes at a time.
            let chunk_size = find_chunk_size(nleaves, P2M_MAX_CHUNK_SIZE);

            check_potentials
                .data()
                .par_chunks_exact(ncoeffs*chunk_size)
                .zip(self.leaf_multipoles.par_chunks_exact(chunk_size))
                .zip(self.scales.par_chunks_exact(ncoeffs*chunk_size))
                .for_each(|((check_potential, multipole_ptrs), scale)| {

                    let check_potential = unsafe { rlst_pointer_mat!['a, V, check_potential.as_ptr(), (ncoeffs, chunk_size), (1, ncoeffs)] };
                    let scale = unsafe {rlst_pointer_mat!['a, V, scale.as_ptr(), (ncoeffs, chunk_size), (1, ncoeffs)]}.eval();

                    let tmp = (self.fmm.uc2e_inv_1.dot(&self.fmm.uc2e_inv_2.dot(&check_potential.cmp_wise_product(&scale)))).eval();

                    for (i, multipole_ptr) in multipole_ptrs.iter().enumerate().take(chunk_size) {
                        let multipole = unsafe { std::slice::from_raw_parts_mut(multipole_ptr.raw, ncoeffs) };
                        multipole.iter_mut().zip(&tmp.data()[i*ncoeffs..(i+1)*ncoeffs]).for_each(|(m, t)| *m += *t);
                    }
                })
        }
    }

    /// Multipole to multipole translations, multithreaded over all boxes at a given level.
    fn m2m<'a>(&self, level: u64) {
        if let Some(child_sources) = self.fmm.tree().get_keys(level) {
            let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);
            let nsiblings = 8;

            // 1. Lookup parents and corresponding children that exist for this set of sources
            //    Must explicitly lookup as boxes may be empty at this level, and the next.
            let parent_targets: HashSet<MortonKey> =
                child_sources.iter().map(|source| source.parent()).collect();
            let mut parent_targets = parent_targets.into_iter().collect_vec();
            parent_targets.sort();
            let nparents = parent_targets.len();
            let mut parent_multipoles = Vec::new();
            for parent in parent_targets.iter() {
                let parent_index_pointer = *self.level_index_pointer[(level - 1) as usize]
                    .get(parent)
                    .unwrap();
                let parent_multipole =
                    self.level_multipoles[(level - 1) as usize][parent_index_pointer];
                parent_multipoles.push(parent_multipole);
            }

            let n_child_sources = child_sources.len();
            let min: &MortonKey = &child_sources[0];
            let max = &child_sources[n_child_sources - 1];
            let min_idx = self.fmm.tree().key_to_index.get(min).unwrap();
            let max_idx = self.fmm.tree().key_to_index.get(max).unwrap();

            let child_multipoles = &self.multipoles[min_idx * ncoeffs..(max_idx + 1) * ncoeffs];

            let mut max_chunk_size = nparents;
            if max_chunk_size > M2M_MAX_CHUNK_SIZE {
                max_chunk_size = M2M_MAX_CHUNK_SIZE
            }
            let chunk_size = find_chunk_size(nparents, max_chunk_size);

            // 3. Compute M2M kernel over sets of siblings
            child_multipoles
                .par_chunks_exact(nsiblings * ncoeffs*chunk_size)
                .zip(parent_multipoles.par_chunks_exact(chunk_size))
                .for_each(|(child_multipoles_chunk, parent_multipole_pointers_chunk)| {
                    let child_multipoles_chunk = unsafe { rlst_pointer_mat!['a, V, child_multipoles_chunk.as_ptr(), (ncoeffs*nsiblings, chunk_size), (1, ncoeffs*nsiblings)] };
                    let parent_multipoles_chunk = self.fmm.m2m.dot(&child_multipoles_chunk).eval();

                    for (chunk_idx, parent_multipole_pointer) in parent_multipole_pointers_chunk.iter().enumerate().take(chunk_size) {
                        let parent_multipole = unsafe { std::slice::from_raw_parts_mut(parent_multipole_pointer.raw, ncoeffs) };
                        parent_multipole.iter_mut().zip(&parent_multipoles_chunk.data()[chunk_idx*ncoeffs..(chunk_idx+1)*ncoeffs]).for_each(|(p, t)| *p += *t);
                    }
                })
        }
    }
}

impl<T, U, V> SourceTranslation for FmmDataAdaptive<KiFmmLinear<SingleNodeTree<V>, T, U, V>, V>
where
    T: Kernel<T = V> + ScaleInvariantKernel<T = V> + std::marker::Send + std::marker::Sync,
    U: FieldTranslationData<T> + std::marker::Sync + std::marker::Send,
    V: Scalar<Real = V> + Float + Default + std::marker::Sync + std::marker::Send,
    V: MultiplyAdd<
        V,
        VectorContainer<V>,
        VectorContainer<V>,
        VectorContainer<V>,
        Dynamic,
        Dynamic,
        Dynamic,
    >,
{
    /// Point to multipole evaluations, multithreaded over each leaf box.
    fn p2m<'a>(&self) {
        if let Some(leaves) = self.fmm.tree().get_all_leaves() {
            let nleaves = leaves.len();
            let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);

            let surface_size = ncoeffs * self.fmm.kernel.space_dimension();

            let mut check_potentials = rlst_col_vec![V, nleaves * ncoeffs];
            let coordinates = self.fmm.tree().get_all_coordinates().unwrap();
            let dim = self.fmm.kernel.space_dimension();

            // 1. Compute the check potential for each box
            check_potentials
                .data_mut()
                .par_chunks_exact_mut(ncoeffs)
                .zip(self.leaf_upward_surfaces.par_chunks_exact(surface_size))
                .zip(&self.charge_index_pointer)
                .for_each(
                    |((check_potential, upward_check_surface), charge_index_pointer)| {
                        let charges = &self.charges[charge_index_pointer.0..charge_index_pointer.1];
                        let coordinates = &coordinates
                            [charge_index_pointer.0 * dim..charge_index_pointer.1 * dim];

                        let nsources = coordinates.len() / dim;

                        if nsources > 0 {
                            let coordinates = unsafe {
                                rlst_pointer_mat!['a, V, coordinates.as_ptr(), (nsources, dim), (dim, 1)]
                            }.eval();

                            self.fmm.kernel.evaluate_st(
                                EvalType::Value,
                                coordinates.data(),
                                upward_check_surface,
                                charges,
                                check_potential,
                            );
                        }
                    },
                );

            // 2. Compute the multipole expansions, with each of chunk_size boxes at a time.
            let chunk_size = find_chunk_size(nleaves, P2M_MAX_CHUNK_SIZE);

            check_potentials
                .data()
                .par_chunks_exact(ncoeffs*chunk_size)
                .zip(self.leaf_multipoles.par_chunks_exact(chunk_size))
                .zip(self.scales.par_chunks_exact(ncoeffs*chunk_size))
                .for_each(|((check_potential, multipole_ptrs), scale)| {

                    let check_potential = unsafe { rlst_pointer_mat!['a, V, check_potential.as_ptr(), (ncoeffs, chunk_size), (1, ncoeffs)] };
                    let scale = unsafe {rlst_pointer_mat!['a, V, scale.as_ptr(), (ncoeffs, chunk_size), (1, ncoeffs)]}.eval();

                    let tmp = (self.fmm.uc2e_inv_1.dot(&self.fmm.uc2e_inv_2.dot(&check_potential.cmp_wise_product(&scale)))).eval();
                    for (i, multipole_ptr) in multipole_ptrs.iter().enumerate().take(chunk_size) {
                        let multipole = unsafe { std::slice::from_raw_parts_mut(multipole_ptr.raw, ncoeffs) };
                        multipole.iter_mut().zip(&tmp.data()[i*ncoeffs..(i+1)*ncoeffs]).for_each(|(m, t)| *m += *t);
                    }
                })
        }
    }

    /// Multipole to multipole translations, multithreaded over all boxes at a given level.
    fn m2m<'a>(&self, level: u64) {
        if let Some(child_sources) = self.fmm.tree().get_keys(level) {
            let ncoeffs = self.fmm.m2l.ncoeffs(self.fmm.order);
            let nsiblings = 8;

            // 1. Lookup parents and corresponding children that exist for this set of sources
            //    Must explicitly lookup as boxes may be empty at this level, and the next.
            let parent_targets: HashSet<MortonKey> =
                child_sources.iter().map(|source| source.parent()).collect();
            let mut parent_targets = parent_targets.into_iter().collect_vec();
            parent_targets.sort();
            let nparents = parent_targets.len();
            let mut parent_multipoles = Vec::new();
            for parent in parent_targets.iter() {
                let parent_index_pointer = *self.level_index_pointer[(level - 1) as usize]
                    .get(parent)
                    .unwrap();
                let parent_multipole =
                    self.level_multipoles[(level - 1) as usize][parent_index_pointer];
                parent_multipoles.push(parent_multipole);
            }

            let n_child_sources = child_sources.len();
            let min: &MortonKey = &child_sources[0];
            let max = &child_sources[n_child_sources - 1];
            let min_idx = self.fmm.tree().key_to_index.get(min).unwrap();
            let max_idx = self.fmm.tree().key_to_index.get(max).unwrap();

            let child_multipoles = &self.multipoles[min_idx * ncoeffs..(max_idx + 1) * ncoeffs];

            let mut max_chunk_size = nparents;
            if max_chunk_size > M2M_MAX_CHUNK_SIZE {
                max_chunk_size = M2M_MAX_CHUNK_SIZE
            }
            let chunk_size = find_chunk_size(nparents, max_chunk_size);

            // 3. Compute M2M kernel over sets of siblings
            child_multipoles
                .par_chunks_exact(nsiblings * ncoeffs*chunk_size)
                .zip(parent_multipoles.par_chunks_exact(chunk_size))
                .for_each(|(child_multipoles_chunk, parent_multipole_pointers_chunk)| {
                    let child_multipoles_chunk = unsafe { rlst_pointer_mat!['a, V, child_multipoles_chunk.as_ptr(), (ncoeffs*nsiblings, chunk_size), (1, ncoeffs*nsiblings)] };
                    let parent_multipoles_chunk = self.fmm.m2m.dot(&child_multipoles_chunk).eval();

                    for (chunk_idx, parent_multipole_pointer) in parent_multipole_pointers_chunk.iter().enumerate().take(chunk_size) {
                        let parent_multipole = unsafe { std::slice::from_raw_parts_mut(parent_multipole_pointer.raw, ncoeffs) };
                        parent_multipole.iter_mut().zip(&parent_multipoles_chunk.data()[chunk_idx*ncoeffs..(chunk_idx+1)*ncoeffs]).for_each(|(p, t)| *p += *t);
                    }
                })
        }
    }
}

#[cfg(test)]
mod test {

    use super::*;

    use itertools::Itertools;

    use crate::charge::build_charge_dict;
    use bempp_field::types::SvdFieldTranslationKiFmm;
    use bempp_kernel::laplace_3d::Laplace3dKernel;
    use bempp_tree::{
        constants::ROOT,
        implementations::helpers::{points_fixture, points_fixture_sphere},
    };

    #[test]
    fn test_upward_pass() {
        let npoints = 10000;
        let points = points_fixture(npoints, None, None);
        let global_idxs = (0..npoints).collect_vec();
        let charges = vec![1.0; npoints];

        let kernel = Laplace3dKernel::<f64>::default();
        let order = 6;
        let alpha_inner = 1.05;
        let alpha_outer = 2.95;
        let adaptive = false;
        let k = 1000;
        let ncrit = 150;
        let depth = 3;

        // Create a tree
        let tree = SingleNodeTree::new(
            points.data(),
            adaptive,
            Some(ncrit),
            Some(depth),
            &global_idxs[..],
            false,
        );

        // Precompute the M2L data
        let m2l_data_svd = SvdFieldTranslationKiFmm::new(
            kernel.clone(),
            Some(k),
            order,
            *tree.get_domain(),
            alpha_inner,
        );
        let fmm = KiFmmLinear::new(order, alpha_inner, alpha_outer, kernel, tree, m2l_data_svd);

        // Form charge dict, matching charges with their associated global indices
        let charge_dict = build_charge_dict(&global_idxs[..], &charges[..]);

        // Associate data with the FMM
        let datatree = FmmDataUniform::new(fmm, &charge_dict).unwrap();

        // Upward pass
        {
            datatree.p2m();

            for level in (1..=depth).rev() {
                datatree.m2m(level);
            }
        }

        let midx = datatree.fmm.tree().key_to_index.get(&ROOT).unwrap();
        let ncoeffs = datatree.fmm.m2l.ncoeffs(datatree.fmm.order);
        let multipole = &datatree.multipoles[midx * ncoeffs..(midx + 1) * ncoeffs];

        let surface =
            ROOT.compute_surface(&datatree.fmm.tree().domain, order, datatree.fmm.alpha_inner);

        let test_point = vec![100000., 0., 0.];

        let mut expected = vec![0.];
        let mut found = vec![0.];

        let kernel = Laplace3dKernel::<f64>::default();
        kernel.evaluate_st(
            EvalType::Value,
            points.data(),
            &test_point,
            &charges,
            &mut expected,
        );

        kernel.evaluate_st(
            EvalType::Value,
            &surface,
            &test_point,
            multipole,
            &mut found,
        );

        let abs_error = (expected[0] - found[0]).abs();
        let rel_error = abs_error / expected[0];
        assert!(rel_error <= 1e-5);
    }

    #[test]
    fn test_upward_pass_sphere() {
        let npoints = 10000;
        let points = points_fixture_sphere(npoints);
        let global_idxs = (0..npoints).collect_vec();
        let charges = vec![1.0; npoints];

        let kernel = Laplace3dKernel::<f64>::default();
        let order = 6;
        let alpha_inner = 1.05;
        let alpha_outer = 2.95;
        let adaptive = false;
        let k = 1000;
        let ncrit = 150;
        let depth = 3;

        // Create a tree
        let tree = SingleNodeTree::new(
            points.data(),
            adaptive,
            Some(ncrit),
            Some(depth),
            &global_idxs[..],
            false,
        );

        // Precompute the M2L data
        let m2l_data_svd = SvdFieldTranslationKiFmm::new(
            kernel.clone(),
            Some(k),
            order,
            *tree.get_domain(),
            alpha_inner,
        );
        let fmm = KiFmmLinear::new(order, alpha_inner, alpha_outer, kernel, tree, m2l_data_svd);

        // Form charge dict, matching charges with their associated global indices
        let charge_dict = build_charge_dict(&global_idxs[..], &charges[..]);

        // Associate data with the FMM
        let datatree = FmmDataUniform::new(fmm, &charge_dict).unwrap();

        // Upward pass
        {
            datatree.p2m();

            for level in (1..=depth).rev() {
                datatree.m2m(level);
            }
        }

        let midx = datatree.fmm.tree().key_to_index.get(&ROOT).unwrap();
        let ncoeffs = datatree.fmm.m2l.ncoeffs(datatree.fmm.order);
        let multipole = &datatree.multipoles[midx * ncoeffs..(midx + 1) * ncoeffs];

        let surface =
            ROOT.compute_surface(&datatree.fmm.tree().domain, order, datatree.fmm.alpha_inner);

        let test_point = vec![100000., 0., 0.];

        let mut expected = vec![0.];
        let mut found = vec![0.];

        let kernel = Laplace3dKernel::<f64>::default();
        kernel.evaluate_st(
            EvalType::Value,
            points.data(),
            &test_point,
            &charges,
            &mut expected,
        );

        kernel.evaluate_st(
            EvalType::Value,
            &surface,
            &test_point,
            multipole,
            &mut found,
        );

        let abs_error = (expected[0] - found[0]).abs();
        let rel_error = abs_error / expected[0];
        assert!(rel_error <= 1e-5);
    }

    #[test]
    fn test_upward_pass_sphere_adaptive() {
        let npoints = 10000;
        let points = points_fixture_sphere(npoints);
        let global_idxs = (0..npoints).collect_vec();
        let charges = vec![1.0; npoints];

        let kernel = Laplace3dKernel::<f64>::default();
        let order = 6;
        let alpha_inner = 1.05;
        let alpha_outer = 2.95;
        let adaptive = true;
        let k = 1000;
        let ncrit = 150;

        // Create a tree
        let tree = SingleNodeTree::new(
            points.data(),
            adaptive,
            Some(ncrit),
            None,
            &global_idxs[..],
            true,
        );

        // Precompute the M2L data
        let m2l_data_svd = SvdFieldTranslationKiFmm::new(
            kernel.clone(),
            Some(k),
            order,
            *tree.get_domain(),
            alpha_inner,
        );
        let fmm = KiFmmLinear::new(order, alpha_inner, alpha_outer, kernel, tree, m2l_data_svd);

        // Form charge dict, matching charges with their associated global indices
        let charge_dict = build_charge_dict(&global_idxs[..], &charges[..]);

        // Associate data with the FMM
        let datatree = FmmDataAdaptive::new(fmm, &charge_dict).unwrap();

        // Upward pass
        {
            datatree.p2m();
            let depth = datatree.fmm.tree().get_depth();
            for level in (1..=depth).rev() {
                datatree.m2m(level);
            }
        }

        let midx = datatree.fmm.tree().key_to_index.get(&ROOT).unwrap();
        let ncoeffs = datatree.fmm.m2l.ncoeffs(datatree.fmm.order);
        let multipole = &datatree.multipoles[midx * ncoeffs..(midx + 1) * ncoeffs];

        let surface =
            ROOT.compute_surface(&datatree.fmm.tree().domain, order, datatree.fmm.alpha_inner);

        let test_point = vec![100000., 0., 0.];

        let mut expected = vec![0.];
        let mut found = vec![0.];

        let kernel = Laplace3dKernel::<f64>::default();
        kernel.evaluate_st(
            EvalType::Value,
            points.data(),
            &test_point,
            &charges,
            &mut expected,
        );

        kernel.evaluate_st(
            EvalType::Value,
            &surface,
            &test_point,
            multipole,
            &mut found,
        );

        let abs_error = (expected[0] - found[0]).abs();
        let rel_error = abs_error / expected[0];
        assert!(rel_error <= 1e-5);
    }
}

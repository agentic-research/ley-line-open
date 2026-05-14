//! Sparse matrix utilities for coboundary computation.
//!
//! Provides O(nnz) sparse matrix-vector and matrix-matrix multiply,
//! sparse-to-dense conversion, and randomized rank estimation via
//! the Halko-Martinsson-Tropp algorithm.

use nalgebra::{DMatrix, DVector, Dyn, SVD};
use nalgebra_sparse::CscMatrix;

/// Maximum dense matrix elements before skipping exact SVD.
/// 10M elements ≈ 40MB — manageable. Beyond this, use randomized estimate.
const MAX_DENSE_ELEMENTS: usize = 10_000_000;

/// Sparse matrix operations for cohomology computation.
pub struct SparseOps;

impl SparseOps {
    /// Sparse matrix × dense vector. O(nnz).
    pub fn spmv(a: &CscMatrix<f32>, x: &DVector<f32>) -> DVector<f32> {
        let mut y = DVector::zeros(a.nrows());
        for col_idx in 0..a.ncols() {
            let x_val = x[col_idx];
            if x_val.abs() > 0.0 {
                let col = a.col(col_idx);
                for (&row_idx, &val) in col.row_indices().iter().zip(col.values().iter()) {
                    y[row_idx] += val * x_val;
                }
            }
        }
        y
    }

    /// Sparse matrix × dense matrix (column-by-column spmv).
    pub fn spmm(a: &CscMatrix<f32>, b: &DMatrix<f32>) -> DMatrix<f32> {
        let mut y = DMatrix::zeros(a.nrows(), b.ncols());
        for j in 0..b.ncols() {
            for col_idx in 0..a.ncols() {
                let b_val = b[(col_idx, j)];
                if b_val.abs() > 0.0 {
                    let col = a.col(col_idx);
                    for (&row_idx, &val) in col.row_indices().iter().zip(col.values().iter()) {
                        y[(row_idx, j)] += val * b_val;
                    }
                }
            }
        }
        y
    }

    /// Convert sparse CSC to dense matrix.
    pub fn to_dense(csc: &CscMatrix<f32>) -> DMatrix<f32> {
        let mut dense = DMatrix::zeros(csc.nrows(), csc.ncols());
        for col_idx in 0..csc.ncols() {
            let col = csc.col(col_idx);
            for (&row_idx, &val) in col.row_indices().iter().zip(col.values().iter()) {
                dense[(row_idx, col_idx)] = val;
            }
        }
        dense
    }

    /// Count significant singular values above numerical threshold.
    pub fn numerical_rank(svd: &SVD<f32, Dyn, Dyn>) -> usize {
        if svd.singular_values.is_empty() {
            return 0;
        }
        let max_sv = svd.singular_values[0];
        let threshold = max_sv * f32::EPSILON * svd.singular_values.len() as f32;
        svd.singular_values
            .iter()
            .filter(|&&s| s > threshold)
            .count()
    }

    /// Estimate rank of a sparse matrix using randomized projection.
    ///
    /// Uses the Halko-Martinsson-Tropp randomized range finder:
    /// 1. Generate pseudo-random Gaussian matrix Ω (ncols × k)
    /// 2. Compute Y = A × Ω via sparse multiply — O(nnz × k)
    /// 3. QR factorize Y (dense but small: nrows × k)
    /// 4. Rank = number of R diagonal entries above numerical threshold
    ///
    /// Falls through to exact dense SVD for small matrices.
    pub fn rank_estimate(mat: &CscMatrix<f32>, max_rank_hint: usize) -> usize {
        let m = mat.nrows();
        let n = mat.ncols();
        if m == 0 || n == 0 {
            return 0;
        }

        // Small matrices: exact dense SVD
        if m * n <= MAX_DENSE_ELEMENTS {
            let dense = Self::to_dense(mat);
            let svd = dense.svd(true, true);
            return Self::numerical_rank(&svd);
        }

        // Oversampling: k = min(max_rank_hint + 20, min(m, n))
        let k = (max_rank_hint + 20).min(m).min(n);

        // Deterministic pseudo-random projection matrix Ω (ncols × k)
        let mut omega = DMatrix::zeros(n, k);
        let mut rng_state: u64 = 42;
        for j in 0..k {
            for i in 0..n {
                // Marsaglia's xorshift64
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                let u = (rng_state as f32) / (u64::MAX as f32);
                omega[(i, j)] = u * 2.0 - 1.0;
            }
        }

        // Y = A × Ω — sparse multiply, result is m × k (small)
        let y = Self::spmm(mat, &omega);

        // QR factorize Y to find rank
        let qr = y.qr();
        let r = qr.r();

        let max_diag = (0..r.nrows().min(r.ncols()))
            .map(|i| r[(i, i)].abs())
            .fold(0.0_f32, f32::max);

        if max_diag < 1e-10 {
            return 0;
        }

        let threshold = max_diag * f32::EPSILON * (m.max(n) as f32);
        (0..r.nrows().min(r.ncols()))
            .filter(|&i| r[(i, i)].abs() > threshold)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra_sparse::CooMatrix;

    #[test]
    fn spmv_identity() {
        // 3×3 identity matrix
        let mut coo = CooMatrix::new(3, 3);
        coo.push(0, 0, 1.0);
        coo.push(1, 1, 1.0);
        coo.push(2, 2, 1.0);
        let csc = CscMatrix::from(&coo);

        let x = DVector::from_vec(vec![1.0, 2.0, 3.0]);
        let y = SparseOps::spmv(&csc, &x);
        assert_eq!(y, x);
    }

    #[test]
    fn rank_of_identity() {
        let mut coo = CooMatrix::new(4, 4);
        for i in 0..4 {
            coo.push(i, i, 1.0);
        }
        let csc = CscMatrix::from(&coo);
        assert_eq!(SparseOps::rank_estimate(&csc, 4), 4);
    }

    #[test]
    fn rank_of_rank_deficient() {
        // 3×3 matrix with rank 2 (row 2 = row 0 + row 1)
        let mut coo = CooMatrix::new(3, 3);
        coo.push(0, 0, 1.0);
        coo.push(0, 1, 0.0);
        coo.push(1, 1, 1.0);
        coo.push(2, 0, 1.0);
        coo.push(2, 1, 1.0);
        let csc = CscMatrix::from(&coo);
        assert_eq!(SparseOps::rank_estimate(&csc, 3), 2);
    }
}

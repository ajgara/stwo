use itertools::{izip, zip_eq};
use num_traits::{One, Zero};

use super::CPUBackend;
use crate::core::backend::{Backend, Col};
use crate::core::circle::CirclePoint;
use crate::core::constraints::{complex_conjugate_line_coeffs, pair_vanishing};
use crate::core::fields::m31::BaseField;
use crate::core::fields::qm31::SecureField;
use crate::core::fields::secure_column::SecureColumn;
use crate::core::fields::{ComplexConjugate, FieldExpOps};
use crate::core::pcs::quotients::{ColumnSampleBatch, PointSample, QuotientOps};
use crate::core::poly::circle::{CircleDomain, CircleEvaluation, SecureEvaluation};
use crate::core::poly::BitReversedOrder;
use crate::core::utils::{bit_reverse, bit_reverse_index};

impl QuotientOps for CPUBackend {
    fn accumulate_quotients(
        domain: CircleDomain,
        columns: &[&CircleEvaluation<Self, BaseField, BitReversedOrder>],
        random_coeff: SecureField,
        sample_batches: &[ColumnSampleBatch],
    ) -> SecureEvaluation<Self> {
        let mut values = SecureColumn::zeros(domain.size());
        let quotient_constants = quotient_constants(sample_batches, random_coeff, domain);

        // TODO(spapini): bit reverse iterator.
        for row in 0..domain.size() {
            // TODO(alonh): Make an efficient bit reverse domain iterator, possibly for AVX backend.
            let domain_point = domain.at(bit_reverse_index(row, domain.log_size()));
            let row_value = accumulate_row_quotients(
                sample_batches,
                columns,
                &quotient_constants,
                row,
                domain_point,
            );
            values.set(row, row_value);
        }
        SecureEvaluation { domain, values }
    }
}

pub fn accumulate_row_quotients(
    sample_batches: &[ColumnSampleBatch],
    columns: &[&CircleEvaluation<CPUBackend, BaseField, BitReversedOrder>],
    quotient_constants: &QuotientConstants<CPUBackend>,
    row: usize,
    domain_point: CirclePoint<BaseField>,
) -> SecureField {
    let mut row_accumulator = SecureField::zero();
    for (sample_batch, line_coeffs, batch_coeff, denominator_inverses) in izip!(
        sample_batches,
        &quotient_constants.line_coeffs,
        &quotient_constants.batch_random_coeffs,
        &quotient_constants.denominator_inverses
    ) {
        let mut numerator = SecureField::zero();
        for ((column_index, _), (a, b, c)) in zip_eq(&sample_batch.columns_and_values, line_coeffs)
        {
            let column = &columns[*column_index];
            let value = column[row] * *c;
            let linear_term = *a * domain_point.y + *b;
            numerator += value - linear_term;
        }

        row_accumulator = row_accumulator * *batch_coeff + numerator * denominator_inverses[row];
    }
    row_accumulator
}

/// Precompute the complex conjugate line coefficients for each column in each sample batch.
/// Specifically, for the i-th (in a sample batch) column's numerator term
/// `alpha^i * (c * F(p) - (a * p.y + b))`, we precompute and return the constants:
/// (`alpha^i * a`, `alpha^i * b`, `alpha^i * c`).
pub fn column_line_coeffs(
    sample_batches: &[ColumnSampleBatch],
    random_coeff: SecureField,
) -> Vec<Vec<(SecureField, SecureField, SecureField)>> {
    sample_batches
        .iter()
        .map(|sample_batch| {
            let mut alpha = SecureField::one();
            sample_batch
                .columns_and_values
                .iter()
                .map(|(_, sampled_value)| {
                    alpha *= random_coeff;
                    let sample = PointSample {
                        point: sample_batch.point,
                        value: *sampled_value,
                    };
                    complex_conjugate_line_coeffs(&sample, alpha)
                })
                .collect()
        })
        .collect()
}

/// Precompute the random coefficients used to linearly combine the batched quotients.
/// Specifically, for each sample batch we compute random_coeff^(number of columns in the batch),
/// which is used to linearly combine the batch with the next one.
pub fn batch_random_coeffs(
    sample_batches: &[ColumnSampleBatch],
    random_coeff: SecureField,
) -> Vec<SecureField> {
    sample_batches
        .iter()
        .map(|sb| random_coeff.pow(sb.columns_and_values.len() as u128))
        .collect()
}

fn denominator_inverses(
    sample_batches: &[ColumnSampleBatch],
    domain: CircleDomain,
) -> Vec<Col<CPUBackend, SecureField>> {
    let mut flat_denominators = Vec::with_capacity(sample_batches.len() * domain.size());
    for sample_batch in sample_batches {
        for row in 0..domain.size() {
            let domain_point = domain.at(row);
            let denominator = pair_vanishing(
                sample_batch.point,
                sample_batch.point.complex_conjugate(),
                domain_point.into_ef(),
            );
            flat_denominators.push(denominator);
        }
    }

    let mut flat_denominator_inverses = vec![SecureField::zero(); flat_denominators.len()];
    SecureField::batch_inverse(&flat_denominators, &mut flat_denominator_inverses);

    flat_denominator_inverses
        .chunks_mut(domain.size())
        .map(|denominator_inverses| {
            bit_reverse(denominator_inverses);
            denominator_inverses.to_vec()
        })
        .collect()
}

pub fn quotient_constants(
    sample_batches: &[ColumnSampleBatch],
    random_coeff: SecureField,
    domain: CircleDomain,
) -> QuotientConstants<CPUBackend> {
    let line_coeffs = column_line_coeffs(sample_batches, random_coeff);
    let batch_random_coeffs = batch_random_coeffs(sample_batches, random_coeff);
    let denominator_inverses = denominator_inverses(sample_batches, domain);
    QuotientConstants {
        line_coeffs,
        batch_random_coeffs,
        denominator_inverses,
    }
}

/// Holds the precomputed constant values used in each quotient evaluation.
pub struct QuotientConstants<B: Backend> {
    /// The line coefficients for each quotient numerator term. For more details see
    /// [self::column_line_coeffs].
    pub line_coeffs: Vec<Vec<(SecureField, SecureField, SecureField)>>,
    /// The random coefficients used to linearly combine the batched quotients For more details see
    /// [self::batch_random_coeffs].
    pub batch_random_coeffs: Vec<SecureField>,
    /// The inverses of the denominators of the quotients.
    pub denominator_inverses: Vec<Col<B, SecureField>>,
}

#[cfg(test)]
mod tests {
    use crate::core::backend::cpu::{CPUCircleEvaluation, CPUCirclePoly};
    use crate::core::backend::CPUBackend;
    use crate::core::circle::SECURE_FIELD_CIRCLE_GEN;
    use crate::core::pcs::quotients::{ColumnSampleBatch, QuotientOps};
    use crate::core::poly::circle::CanonicCoset;
    use crate::{m31, qm31};

    #[test]
    fn test_quotients_are_low_degree() {
        const LOG_SIZE: u32 = 7;
        let polynomial = CPUCirclePoly::new((0..1 << LOG_SIZE).map(|i| m31!(i)).collect());
        let eval_domain = CanonicCoset::new(LOG_SIZE + 1).circle_domain();
        let eval = polynomial.evaluate(eval_domain);
        let point = SECURE_FIELD_CIRCLE_GEN;
        let value = polynomial.eval_at_point(point);
        let coeff = qm31!(1, 2, 3, 4);
        let quot_eval = CPUBackend::accumulate_quotients(
            eval_domain,
            &[&eval],
            coeff,
            &[ColumnSampleBatch {
                point,
                columns_and_values: vec![(0, value)],
            }],
        );
        let quot_poly_base_field =
            CPUCircleEvaluation::new(eval_domain, quot_eval.columns[0].clone()).interpolate();
        assert!(quot_poly_base_field.is_in_fft_space(LOG_SIZE));
    }
}

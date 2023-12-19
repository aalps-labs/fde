// NOTE code mostly taken from https://github.com/roynalnaruto/range_proof
mod poly;
mod utils;

use crate::commit::kzg::{Kzg, Powers};
use crate::hash::Hasher;
use ark_ec::pairing::Pairing;
use ark_ec::{AffineRepr, CurveGroup};
use ark_poly::{EvaluationDomain, GeneralEvaluationDomain, Polynomial};
use ark_std::marker::PhantomData;
use ark_std::rand::Rng;
use ark_std::UniformRand;
use digest::Digest;

const PROOF_DOMAIN_SEP: &[u8] = b"fde range proof";

pub struct Evaluations<S> {
    pub g: S,
    pub g_omega: S,
    pub w_cap: S,
}

pub struct Commitments<C: Pairing> {
    pub f: C::G1Affine,
    pub g: C::G1Affine,
    pub q: C::G1Affine,
}

pub struct Proofs<C: Pairing> {
    pub aggregate: C::G1Affine,
    pub shifted: C::G1Affine,
}

pub struct RangeProof<C: Pairing, D> {
    pub evaluations: Evaluations<C::ScalarField>,
    pub commitments: Commitments<C>,
    pub proofs: Proofs<C>,
    _digest: PhantomData<D>,
}

impl<C: Pairing, D: Digest> RangeProof<C, D> {
    // prove 0 <= z < 2^n
    pub fn new<R: Rng>(z: C::ScalarField, n: usize, powers: &Powers<C>, rng: &mut R) -> Self {
        let domain = GeneralEvaluationDomain::<C::ScalarField>::new(n).expect("valid domain");
        let domain_2n =
            GeneralEvaluationDomain::<C::ScalarField>::new(2 * n).expect("valid domain");

        // random scalars
        let r = C::ScalarField::rand(rng);
        let alpha = C::ScalarField::rand(rng);
        let beta = C::ScalarField::rand(rng);

        // compute f and g polynomials and their commitments
        let f_poly = poly::f(&domain, z, r);
        let g_poly = poly::g(&domain, z, alpha, beta);
        let f_commitment = powers.commit_g1(&f_poly);
        let g_commitment = powers.commit_g1(&g_poly);

        // compute challenges
        let mut hasher = Hasher::<D>::new();
        hasher.update(&PROOF_DOMAIN_SEP);
        hasher.update(&n.to_le_bytes());
        hasher.update(&domain.group_gen());
        hasher.update(&f_commitment);
        hasher.update(&g_commitment);

        let tau = hasher.next_scalar(b"tau");
        let rho = hasher.next_scalar(b"rho");
        let aggregation_challenge = hasher.next_scalar(b"aggregation_challenge");

        // aggregate w1, w2 and w3 to compute quotient polynomial
        let (w1_poly, w2_poly) = poly::w1_w2(&domain, &f_poly, &g_poly);
        let w3_poly = poly::w3(&domain, &domain_2n, &g_poly);
        let q_poly = poly::quotient(&domain, &w1_poly, &w2_poly, &w3_poly, tau);
        let q_commitment = powers.commit_g1(&q_poly);

        let rho_omega = rho * domain.group_gen();
        // evaluate g at rho
        let g_eval = g_poly.evaluate(&rho);
        // evaluate g at `rho * omega`
        let g_omega_eval = g_poly.evaluate(&rho_omega);

        // compute evaluation of w_cap at ρ
        let w_cap_poly = poly::w_cap(&domain, &f_poly, &q_poly, rho);
        let w_cap_eval = w_cap_poly.evaluate(&rho);

        // compute witness for g(X) at ρw
        let shifted_witness_poly = Kzg::<C>::witness(&g_poly, rho_omega);
        let shifted_proof = powers.commit_g1(&shifted_witness_poly);

        // compute aggregate witness for
        // g(X) at ρ, f(X) at ρ, w_cap(X) at ρ
        let aggregate_witness_poly =
            Kzg::<C>::aggregate_witness(&[g_poly, w_cap_poly], rho, aggregation_challenge);
        let aggregate_proof = powers.commit_g1(&aggregate_witness_poly);

        let evaluations = Evaluations {
            g: g_eval,
            g_omega: g_omega_eval,
            w_cap: w_cap_eval,
        };

        let commitments = Commitments {
            f: f_commitment.into_affine(),
            g: g_commitment.into_affine(),
            q: q_commitment.into_affine(),
        };

        let proofs = Proofs {
            aggregate: aggregate_proof.into_affine(),
            shifted: shifted_proof.into_affine(),
        };

        Self {
            evaluations,
            commitments,
            proofs,
            _digest: PhantomData,
        }
    }

    pub fn verify(&self, n: usize, powers: &Powers<C>) -> bool {
        let domain = GeneralEvaluationDomain::<C::ScalarField>::new(n).expect("valid domain");

        let mut hasher = Hasher::<D>::new();
        hasher.update(&PROOF_DOMAIN_SEP);
        hasher.update(&n.to_le_bytes());
        hasher.update(&domain.group_gen());
        hasher.update(&self.commitments.f);
        hasher.update(&self.commitments.g);

        let tau = hasher.next_scalar(b"tau");
        let rho = hasher.next_scalar(b"rho");
        let aggregation_challenge: C::ScalarField = hasher.next_scalar(b"aggregation_challenge");

        // calculate w_cap_commitment
        let w_cap_commitment =
            utils::w_cap::<C::G1>(domain.size(), self.commitments.f, self.commitments.q, rho);

        // calculate w2(ρ) and w3(ρ)
        let sum = utils::w1_w2_w3_evals_sum(
            &domain,
            self.evaluations.g,
            self.evaluations.g_omega,
            rho,
            tau,
        );
        // calculate w(ρ) that should zero since w(X) is after all a zero polynomial
        if sum != self.evaluations.w_cap {
            return false;
        }

        // check aggregate witness commitment
        let aggregate_poly_commitment = utils::aggregate(
            &[
                self.commitments.g.into_group(),
                w_cap_commitment.into_group(),
            ],
            aggregation_challenge,
        );
        let aggregate_value = utils::aggregate(
            &[self.evaluations.g, self.evaluations.w_cap],
            aggregation_challenge,
        );
        let aggregation_kzg_check = Kzg::verify_scalar(
            self.proofs.aggregate,
            aggregate_poly_commitment.into_affine(),
            rho,
            aggregate_value,
            powers,
        );

        // check shifted witness commitment
        let rho_omega = rho * domain.group_gen();
        let shifted_kzg_check = Kzg::verify_scalar(
            self.proofs.shifted,
            self.commitments.g,
            rho_omega,
            self.evaluations.g_omega,
            powers,
        );

        aggregation_kzg_check && shifted_kzg_check
    }
}

#[cfg(test)]
mod test {
    use crate::commit::kzg::Powers;
    use crate::tests::{BlsCurve, RangeProof, Scalar};
    use ark_std::{test_rng, UniformRand};

    const LOG_2_UPPER_BOUND: usize = 8; // 2^8

    #[test]
    fn range_proof_success() {
        // KZG setup simulation
        let rng = &mut test_rng();
        let tau = Scalar::rand(rng); // "secret" tau
        let powers = Powers::<BlsCurve>::unsafe_setup(tau, 4 * LOG_2_UPPER_BOUND);

        let z = Scalar::from(100u32);
        let proof = RangeProof::new(z, LOG_2_UPPER_BOUND, &powers, rng);
        assert!(proof.verify(LOG_2_UPPER_BOUND, &powers));

        let z = Scalar::from(255u32);
        let proof = RangeProof::new(z, LOG_2_UPPER_BOUND, &powers, rng);
        assert!(proof.verify(LOG_2_UPPER_BOUND, &powers));
    }

    #[test]
    fn range_proof_with_invalid_size_fails() {
        // KZG setup simulation
        let rng = &mut test_rng();
        let tau = Scalar::rand(rng); // "secret" tau
        let powers = Powers::<BlsCurve>::unsafe_setup(tau, 4 * LOG_2_UPPER_BOUND);

        let z = Scalar::from(100u32);
        let proof = RangeProof::new(z, LOG_2_UPPER_BOUND, &powers, rng);
        assert!(!proof.verify(LOG_2_UPPER_BOUND - 1, &powers));
    }

    #[test]
    #[should_panic(expected = "remainder poly should be zero")]
    fn range_proof_with_too_large_z_fails_1() {
        // KZG setup simulation
        let rng = &mut test_rng();
        let tau = Scalar::rand(rng); // "secret" tau
        let powers = Powers::<BlsCurve>::unsafe_setup(tau, 4 * LOG_2_UPPER_BOUND);

        let z = Scalar::from(256u32);
        let proof = RangeProof::new(z, LOG_2_UPPER_BOUND, &powers, rng);
        assert!(!proof.verify(LOG_2_UPPER_BOUND - 1, &powers));
    }

    #[test]
    #[should_panic(expected = "remainder poly should be zero")]
    fn range_proof_with_too_large_z_fails_2() {
        // KZG setup simulation
        let rng = &mut test_rng();
        let tau = Scalar::rand(rng); // "secret" tau
        let powers = Powers::<BlsCurve>::unsafe_setup(tau, 4 * LOG_2_UPPER_BOUND);

        let z = Scalar::from(300u32);
        let proof = RangeProof::new(z, LOG_2_UPPER_BOUND, &powers, rng);
        assert!(!proof.verify(LOG_2_UPPER_BOUND - 1, &powers));
    }
}

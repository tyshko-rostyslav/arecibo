//! This library implements Nova, a high-speed recursive SNARK.
#![deny(
  warnings,
  unused,
  future_incompatible,
  nonstandard_style,
  rust_2018_idioms,
  missing_docs
)]
#![allow(non_snake_case)]
// #![forbid(unsafe_code)] // Commented for development with `Abomonation`

// private modules
mod bellpepper;
mod circuit;
mod digest;
mod nifs;

// public modules
pub mod constants;
pub mod errors;
pub mod gadgets;
pub mod provider;
pub mod r1cs;
pub mod spartan;
pub mod traits;

pub mod supernova;

use once_cell::sync::OnceCell;

use crate::bellpepper::{r1cs::NovaShape, shape_cs::ShapeCS, solver::WitnessViewCS};
use crate::digest::{DigestComputer, SimpleDigestible};
use crate::r1cs::default_T;
use abomonation::Abomonation;
use abomonation_derive::Abomonation;
use circuit::{NovaAugmentedCircuit, NovaAugmentedCircuitInputs, NovaAugmentedCircuitParams};
use constants::{BN_LIMB_WIDTH, BN_N_LIMBS, NUM_FE_WITHOUT_IO_FOR_CRHF, NUM_HASH_BITS};
use core::marker::PhantomData;
use errors::NovaError;
use ff::{Field, PrimeField};
use gadgets::utils::scalar_as_base;
use nifs::NIFS;
use r1cs::{
  CommitmentKeyHint, R1CSInstance, R1CSShape, R1CSWitness, RelaxedR1CSInstance, RelaxedR1CSWitness,
};
use serde::{Deserialize, Serialize};
use traits::{
  circuit::StepCircuit,
  commitment::{CommitmentEngineTrait, CommitmentTrait},
  snark::RelaxedR1CSSNARKTrait,
  AbsorbInROTrait, Group, ROConstants, ROConstantsCircuit, ROTrait,
};

/// A type that holds parameters for the primary and secondary circuits of Nova and SuperNova
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(where <G::Scalar as PrimeField>::Repr: Abomonation)]
pub struct CircuitShape<G: Group> {
  F_arity: usize,
  r1cs_shape: R1CSShape<G>,
}

impl<G: Group> SimpleDigestible for CircuitShape<G> {}

impl<G: Group> CircuitShape<G> {
  /// Create a new `CircuitShape`
  pub fn new(r1cs_shape: R1CSShape<G>, F_arity: usize) -> Self {
    Self {
      F_arity,
      r1cs_shape,
    }
  }

  /// Return the [CircuitShape]' digest.
  pub fn digest(&self) -> G::Scalar {
    let dc: DigestComputer<'_, <G as Group>::Scalar, CircuitShape<G>> = DigestComputer::new(self);
    dc.digest().expect("Failure in computing digest")
  }
}

/// A type that holds public parameters of Nova
#[derive(Clone, PartialEq, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  <G1::Scalar as PrimeField>::Repr: Abomonation,
  <G2::Scalar as PrimeField>::Repr: Abomonation,
)]
pub struct PublicParams<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<G1>,
  ro_consts_circuit_primary: ROConstantsCircuit<G2>,
  ck_primary: CommitmentKey<G1>,
  circuit_shape_primary: CircuitShape<G1>,
  ro_consts_secondary: ROConstants<G2>,
  ro_consts_circuit_secondary: ROConstantsCircuit<G1>,
  ck_secondary: CommitmentKey<G2>,
  circuit_shape_secondary: CircuitShape<G2>,
  augmented_circuit_params_primary: NovaAugmentedCircuitParams,
  augmented_circuit_params_secondary: NovaAugmentedCircuitParams,
  #[abomonation_skip]
  #[serde(skip, default = "OnceCell::new")]
  digest: OnceCell<G1::Scalar>,
  _p: PhantomData<(C1, C2)>,
}

impl<G1, G2, C1, C2> SimpleDigestible for PublicParams<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
}

impl<G1, G2, C1, C2> PublicParams<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  /// Set up builder to create `PublicParams` for a pair of circuits `C1` and `C2`.
  ///
  /// # Note
  ///
  /// Public parameters set up a number of bases for the homomorphic commitment scheme of Nova.
  ///
  /// Some final compressing SNARKs, like variants of Spartan, use computation commitments that require
  /// larger sizes for these parameters. These SNARKs provide a hint for these values by
  /// implementing `RelaxedR1CSSNARKTrait::ck_floor()`, which can be passed to this function.
  ///
  /// If you're not using such a SNARK, pass `nova_snark::traits::snark::default_ck_hint()` instead.
  ///
  /// # Arguments
  ///
  /// * `c_primary`: The primary circuit of type `C1`.
  /// * `c_secondary`: The secondary circuit of type `C2`.
  /// * `ck_hint1`: A `CommitmentKeyHint` for `G1`, which is a function that provides a hint
  ///   for the number of generators required in the commitment scheme for the primary circuit.
  /// * `ck_hint2`: A `CommitmentKeyHint` for `G2`, similar to `ck_hint1`, but for the secondary circuit.
  ///
  /// # Example
  ///
  /// ```rust
  /// # use pasta_curves::{vesta, pallas};
  /// # use nova_snark::spartan::ppsnark::RelaxedR1CSSNARK;
  /// # use nova_snark::provider::ipa_pc::EvaluationEngine;
  /// # use nova_snark::traits::{circuit::TrivialCircuit, Group, snark::RelaxedR1CSSNARKTrait};
  /// use nova_snark::PublicParams;
  ///
  /// type G1 = pallas::Point;
  /// type G2 = vesta::Point;
  /// type EE<G> = EvaluationEngine<G>;
  /// type SPrime<G> = RelaxedR1CSSNARK<G, EE<G>>;
  ///
  /// let circuit1 = TrivialCircuit::<<G1 as Group>::Scalar>::default();
  /// let circuit2 = TrivialCircuit::<<G2 as Group>::Scalar>::default();
  /// // Only relevant for a SNARK using computational commitments, pass &(|_| 0)
  /// // or &*nova_snark::traits::snark::default_ck_hint() otherwise.
  /// let ck_hint1 = &*SPrime::<G1>::ck_floor();
  /// let ck_hint2 = &*SPrime::<G2>::ck_floor();
  ///
  /// let pp = PublicParams::new(&circuit1, &circuit2, ck_hint1, ck_hint2);
  /// ```
  pub fn new(
    c_primary: &C1,
    c_secondary: &C2,
    ck_hint1: &CommitmentKeyHint<G1>,
    ck_hint2: &CommitmentKeyHint<G2>,
  ) -> Self {
    let augmented_circuit_params_primary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, true);
    let augmented_circuit_params_secondary =
      NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, false);

    let ro_consts_primary: ROConstants<G1> = ROConstants::<G1>::default();
    let ro_consts_secondary: ROConstants<G2> = ROConstants::<G2>::default();

    let F_arity_primary = c_primary.arity();
    let F_arity_secondary = c_secondary.arity();

    // ro_consts_circuit_primary are parameterized by G2 because the type alias uses G2::Base = G1::Scalar
    let ro_consts_circuit_primary: ROConstantsCircuit<G2> = ROConstantsCircuit::<G2>::default();
    let ro_consts_circuit_secondary: ROConstantsCircuit<G1> = ROConstantsCircuit::<G1>::default();

    // Initialize ck for the primary
    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_primary,
      None,
      c_primary,
      ro_consts_circuit_primary.clone(),
    );
    let mut cs: ShapeCS<G1> = ShapeCS::new();
    let _ = circuit_primary.synthesize(&mut cs);
    let (r1cs_shape_primary, ck_primary) = cs.r1cs_shape_and_key(ck_hint1);
    let circuit_shape_primary = CircuitShape::new(r1cs_shape_primary, F_arity_primary);

    // Initialize ck for the secondary
    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &augmented_circuit_params_secondary,
      None,
      c_secondary,
      ro_consts_circuit_secondary.clone(),
    );
    let mut cs: ShapeCS<G2> = ShapeCS::new();
    let _ = circuit_secondary.synthesize(&mut cs);
    let (r1cs_shape_secondary, ck_secondary) = cs.r1cs_shape_and_key(ck_hint2);
    let circuit_shape_secondary = CircuitShape::new(r1cs_shape_secondary, F_arity_secondary);

    PublicParams {
      F_arity_primary,
      F_arity_secondary,
      ro_consts_primary,
      ro_consts_circuit_primary,
      ck_primary,
      circuit_shape_primary,
      ro_consts_secondary,
      ro_consts_circuit_secondary,
      ck_secondary,
      circuit_shape_secondary,
      augmented_circuit_params_primary,
      augmented_circuit_params_secondary,
      digest: OnceCell::new(),
      _p: Default::default(),
    }
  }

  /// Retrieve the digest of the public parameters.
  pub fn digest(&self) -> G1::Scalar {
    self
      .digest
      .get_or_try_init(|| DigestComputer::new(self).digest())
      .cloned()
      .expect("Failure in retrieving digest")
  }

  /// Returns the number of constraints in the primary and secondary circuits
  pub const fn num_constraints(&self) -> (usize, usize) {
    (
      self.circuit_shape_primary.r1cs_shape.num_cons,
      self.circuit_shape_secondary.r1cs_shape.num_cons,
    )
  }

  /// Returns the number of variables in the primary and secondary circuits
  pub const fn num_variables(&self) -> (usize, usize) {
    (
      self.circuit_shape_primary.r1cs_shape.num_vars,
      self.circuit_shape_secondary.r1cs_shape.num_vars,
    )
  }
}

/// A SNARK that proves the correct execution of an incremental computation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct RecursiveSNARK<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  z0_primary: Vec<G1::Scalar>,
  z0_secondary: Vec<G2::Scalar>,

  r_W_primary: RelaxedR1CSWitness<G1>,
  r_U_primary: RelaxedR1CSInstance<G1>,
  r_W_secondary: RelaxedR1CSWitness<G2>,
  r_U_secondary: RelaxedR1CSInstance<G2>,

  l_w_primary: R1CSWitness<G1>,
  l_u_primary: R1CSInstance<G1>,
  l_w_secondary: R1CSWitness<G2>,
  l_u_secondary: R1CSInstance<G2>,

  /// buffer for `commit_T`
  T_primary: Vec<G1::Scalar>,
  /// buffer for `commit_T`
  T_secondary: Vec<G2::Scalar>,

  i: usize,
  zi_primary: Vec<G1::Scalar>,
  zi_secondary: Vec<G2::Scalar>,
  _p: PhantomData<(C1, C2)>,
}

impl<G1, G2, C1, C2> RecursiveSNARK<G1, G2, C1, C2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
{
  /// Create new instance of recursive SNARK
  pub fn new(
    pp: &PublicParams<G1, G2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
    z0_primary: &[G1::Scalar],
    z0_secondary: &[G2::Scalar],
  ) -> Result<Self, NovaError> {
    if z0_primary.len() != pp.F_arity_primary || z0_secondary.len() != pp.F_arity_secondary {
      return Err(NovaError::InvalidInitialInputLength);
    }

    let r1cs_primary = &pp.circuit_shape_primary.r1cs_shape;
    let r1cs_secondary = &pp.circuit_shape_secondary.r1cs_shape;

    // base case for the primary
    let (mut input_assignment, mut aux_assignment) = (Vec::new(), Vec::new());
    let mut cs_primary =
      WitnessViewCS::<G1::Scalar>::new_view(&mut input_assignment, &mut aux_assignment);
    let inputs_primary: NovaAugmentedCircuitInputs<G2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<G1>(pp.digest()),
      G1::Scalar::ZERO,
      z0_primary.to_vec(),
      None,
      None,
      None,
      None,
    );

    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );
    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");

    let (u_primary, w_primary) = r1cs::instance_and_witness(
      r1cs_primary,
      &pp.ck_primary,
      input_assignment,
      aux_assignment,
    )?;

    // base case for the secondary
    let (mut input_assignment, mut aux_assignment) = (Vec::new(), Vec::new());
    let mut cs_secondary =
      WitnessViewCS::<G2::Scalar>::new_view(&mut input_assignment, &mut aux_assignment);
    let inputs_secondary: NovaAugmentedCircuitInputs<G1> = NovaAugmentedCircuitInputs::new(
      pp.digest(),
      G2::Scalar::ZERO,
      z0_secondary.to_vec(),
      None,
      None,
      Some(u_primary.clone()),
      None,
    );
    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)
      .expect("Nova error synthesis");

    let (u_secondary, w_secondary) = r1cs::instance_and_witness(
      r1cs_secondary,
      &pp.ck_secondary,
      input_assignment,
      aux_assignment,
    )?;

    // IVC proof for the primary circuit
    let l_w_primary = R1CSWitness::default(r1cs_primary);
    let l_u_primary = R1CSInstance::default(&pp.ck_primary, r1cs_primary);
    let r_W_primary = RelaxedR1CSWitness::from_r1cs_witness(r1cs_primary, w_primary);
    let r_U_primary =
      RelaxedR1CSInstance::from_r1cs_instance(&pp.ck_primary, r1cs_primary, u_primary);

    // IVC proof for the secondary circuit
    let l_w_secondary = w_secondary;
    let l_u_secondary = u_secondary;
    let r_W_secondary = RelaxedR1CSWitness::<G2>::default(r1cs_secondary);
    let r_U_secondary = RelaxedR1CSInstance::<G2>::default(&pp.ck_secondary, r1cs_secondary);

    assert!(
      !(zi_primary.len() != pp.F_arity_primary || zi_secondary.len() != pp.F_arity_secondary),
      "Invalid step length"
    );

    let zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G1 as Group>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    let zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G2 as Group>::Scalar>, NovaError>>()
      .expect("Nova error synthesis");

    let mut recursive_snark = Self {
      z0_primary: z0_primary.to_vec(),
      z0_secondary: z0_secondary.to_vec(),

      r_W_primary,
      r_U_primary,
      r_W_secondary,
      r_U_secondary,

      l_w_primary,
      l_u_primary,
      l_w_secondary,
      l_u_secondary,

      T_primary: default_T(r1cs_primary),
      T_secondary: default_T(r1cs_secondary),

      i: 0,
      zi_primary,
      zi_secondary,
      _p: Default::default(),
    };

    // resize the witness buffers to be as snug as possible
    recursive_snark.shrink_to_fit();

    Ok(recursive_snark)
  }

  /// Shrink the witness buffers to the exact size they need to be
  fn shrink_to_fit(&mut self) {
    self.r_W_primary.W.shrink_to_fit();
    self.r_U_primary.u_and_X.shrink_to_fit();
    self.r_W_secondary.W.shrink_to_fit();
    self.r_U_secondary.u_and_X.shrink_to_fit();
  }

  /// Create a new `RecursiveSNARK` (or updates the provided `RecursiveSNARK`)
  /// by executing a step of the incremental computation
  #[tracing::instrument(skip_all, name = "nova::RecursiveSNARK::prove_step")]
  pub fn prove_step(
    &mut self,
    pp: &PublicParams<G1, G2, C1, C2>,
    c_primary: &C1,
    c_secondary: &C2,
  ) -> Result<(), NovaError> {
    // first step was already done in the constructor
    if self.i == 0 {
      self.i = 1;
      return Ok(());
    }

    // save the inputs before proceeding to the `i+1`th step
    let r_U_primary_i = self.r_U_primary.clone();
    let r_U_secondary_i = self.r_U_secondary.clone();
    // let l_u_primary_i = self.l_u_primary.clone();
    let l_u_secondary_i = self.l_u_secondary.clone();

    // fold the secondary circuit's instance
    // increments `r_U_secondary` and `r_W_secondary`
    let nifs_secondary = NIFS::prove_mut(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<G1>(pp.digest()),
      &pp.circuit_shape_secondary.r1cs_shape,
      &mut self.r_U_secondary,
      &mut self.r_W_secondary,
      &self.l_u_secondary,
      &self.l_w_secondary,
      &mut self.T_secondary,
    )
    .expect("Unable to fold secondary");

    // increment `l_u_primary` and `l_w_primary`
    let mut cs_primary = WitnessViewCS::<G1::Scalar>::new_view(
      &mut self.l_u_primary.one_and_X,
      &mut self.l_w_primary.W,
    );
    let inputs_primary: NovaAugmentedCircuitInputs<G2> = NovaAugmentedCircuitInputs::new(
      scalar_as_base::<G1>(pp.digest()),
      G1::Scalar::from(self.i as u64),
      self.z0_primary.to_vec(),
      Some(self.zi_primary.clone()),
      Some(r_U_secondary_i),
      Some(l_u_secondary_i),
      Some(Commitment::<G2>::decompress(&nifs_secondary.comm_T)?),
    );

    let circuit_primary: NovaAugmentedCircuit<'_, G2, C1> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_primary,
      Some(inputs_primary),
      c_primary,
      pp.ro_consts_circuit_primary.clone(),
    );

    let zi_primary = circuit_primary
      .synthesize(&mut cs_primary)
      .map_err(|_| NovaError::SynthesisError)?;

    // TODO: check length of witness
    // let (l_u_primary, l_w_primary) = cs_primary
    //   .r1cs_instance_and_witness(&pp.circuit_shape_primary.r1cs_shape, &pp.ck_primary)
    //   .map_err(|_e| NovaError::UnSat)
    //   .expect("Nova error unsat");
    self.l_u_primary.comm_W = self.l_w_primary.commit(&pp.ck_primary);

    // fold the primary circuit's instance
    let nifs_primary = NIFS::prove_mut(
      &pp.ck_primary,
      &pp.ro_consts_primary,
      &pp.digest(),
      &pp.circuit_shape_primary.r1cs_shape,
      &mut self.r_U_primary,
      &mut self.r_W_primary,
      &self.l_u_primary,
      &self.l_w_primary,
      &mut self.T_primary,
    )
    .expect("Unable to fold primary");

    // increment `l_u_secondary` and `l_w_secondary`
    let mut cs_secondary = WitnessViewCS::<G2::Scalar>::new_view(
      &mut self.l_u_secondary.one_and_X,
      &mut self.l_w_secondary.W,
    );
    let inputs_secondary: NovaAugmentedCircuitInputs<G1> = NovaAugmentedCircuitInputs::new(
      pp.digest(),
      G2::Scalar::from(self.i as u64),
      self.z0_secondary.to_vec(),
      Some(self.zi_secondary.clone()),
      Some(r_U_primary_i),
      Some(self.l_u_primary.clone()),
      Some(Commitment::<G1>::decompress(&nifs_primary.comm_T)?),
    );

    let circuit_secondary: NovaAugmentedCircuit<'_, G1, C2> = NovaAugmentedCircuit::new(
      &pp.augmented_circuit_params_secondary,
      Some(inputs_secondary),
      c_secondary,
      pp.ro_consts_circuit_secondary.clone(),
    );
    let zi_secondary = circuit_secondary
      .synthesize(&mut cs_secondary)
      .map_err(|_| NovaError::SynthesisError)?;

    // TODO: check length of witness
    // let (l_u_secondary, l_w_secondary) = cs_secondary
    //   .r1cs_instance_and_witness(&pp.circuit_shape_secondary.r1cs_shape, &pp.ck_secondary)
    //   .map_err(|_e| NovaError::UnSat)?;
    self.l_u_secondary.comm_W = self.l_w_secondary.commit(&pp.ck_secondary);

    // update the running instances and witnesses
    self.zi_primary = zi_primary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G1 as Group>::Scalar>, NovaError>>()?;
    self.zi_secondary = zi_secondary
      .iter()
      .map(|v| v.get_value().ok_or(NovaError::SynthesisError))
      .collect::<Result<Vec<<G2 as Group>::Scalar>, NovaError>>()?;

    self.i += 1;
    Ok(())
  }

  /// Verify the correctness of the `RecursiveSNARK`
  pub fn verify(
    &self,
    pp: &PublicParams<G1, G2, C1, C2>,
    num_steps: usize,
    z0_primary: &[G1::Scalar],
    z0_secondary: &[G2::Scalar],
  ) -> Result<(Vec<G1::Scalar>, Vec<G2::Scalar>), NovaError> {
    // number of steps cannot be zero
    let is_num_steps_zero = num_steps == 0;

    // check if the provided proof has executed num_steps
    let is_num_steps_not_match = self.i != num_steps;

    // check if the initial inputs match
    let is_inputs_not_match = self.z0_primary != z0_primary || self.z0_secondary != z0_secondary;

    // check if the (relaxed) R1CS instances have two public outputs
    let is_instance_has_two_outputs = self.l_u_secondary.one_and_X.len() != 3
      || self.l_u_secondary.one_and_X[0] != G2::Scalar::ONE
      || self.r_U_primary.u_and_X.len() != 3
      || self.r_U_secondary.u_and_X.len() != 3;

    if is_num_steps_zero
      || is_num_steps_not_match
      || is_inputs_not_match
      || is_instance_has_two_outputs
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <<G2 as Group>::RO as ROTrait<G2::Base, G2::Scalar>>::new(
        pp.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_primary,
      );
      hasher.absorb(pp.digest());
      hasher.absorb(G1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(*e);
      }
      for e in &self.zi_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <<G1 as Group>::RO as ROTrait<G1::Base, G1::Scalar>>::new(
        pp.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * pp.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<G1>(pp.digest()));
      hasher2.absorb(G2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(*e);
      }
      for e in &self.zi_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.one_and_X[1]
      || hash_secondary != scalar_as_base::<G2>(self.l_u_secondary.one_and_X[2])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check the satisfiability of the provided instances
    let (res_r_primary, (res_r_secondary, res_l_secondary)) = rayon::join(
      || {
        pp.circuit_shape_primary.r1cs_shape.is_sat_relaxed(
          &pp.ck_primary,
          &self.r_U_primary,
          &self.r_W_primary,
        )
      },
      || {
        rayon::join(
          || {
            pp.circuit_shape_secondary.r1cs_shape.is_sat_relaxed(
              &pp.ck_secondary,
              &self.r_U_secondary,
              &self.r_W_secondary,
            )
          },
          || {
            pp.circuit_shape_secondary.r1cs_shape.is_sat(
              &pp.ck_secondary,
              &self.l_u_secondary,
              &self.l_w_secondary,
            )
          },
        )
      },
    );

    // check the returned res objects
    res_r_primary?;
    res_r_secondary?;
    res_l_secondary?;

    Ok((self.zi_primary.clone(), self.zi_secondary.clone()))
  }
}

/// A type that holds the prover key for `CompressedSNARK`
#[derive(Clone, Debug, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_omit_bounds]
pub struct ProverKey<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  pk_primary: S1::ProverKey,
  pk_secondary: S2::ProverKey,
  _p: PhantomData<(C1, C2)>,
}

/// A type that holds the verifier key for `CompressedSNARK`
#[derive(Clone, Serialize, Deserialize, Abomonation)]
#[serde(bound = "")]
#[abomonation_bounds(
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
  <G1::Scalar as PrimeField>::Repr: Abomonation,
)]
pub struct VerifierKey<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  F_arity_primary: usize,
  F_arity_secondary: usize,
  ro_consts_primary: ROConstants<G1>,
  ro_consts_secondary: ROConstants<G2>,
  #[abomonate_with(<G1::Scalar as PrimeField>::Repr)]
  pp_digest: G1::Scalar,
  vk_primary: S1::VerifierKey,
  vk_secondary: S2::VerifierKey,
  _p: PhantomData<(C1, C2)>,
}

/// A SNARK that proves the knowledge of a valid `RecursiveSNARK`
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct CompressedSNARK<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  r_U_primary: RelaxedR1CSInstance<G1>,
  r_W_snark_primary: S1,

  r_U_secondary: RelaxedR1CSInstance<G2>,
  l_u_secondary: R1CSInstance<G2>,
  nifs_secondary: NIFS<G2>,
  f_W_snark_secondary: S2,

  zn_primary: Vec<G1::Scalar>,
  zn_secondary: Vec<G2::Scalar>,

  _p: PhantomData<(C1, C2)>,
}

impl<G1, G2, C1, C2, S1, S2> CompressedSNARK<G1, G2, C1, C2, S1, S2>
where
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C1: StepCircuit<G1::Scalar>,
  C2: StepCircuit<G2::Scalar>,
  S1: RelaxedR1CSSNARKTrait<G1>,
  S2: RelaxedR1CSSNARKTrait<G2>,
{
  /// Creates prover and verifier keys for `CompressedSNARK`
  pub fn setup(
    pp: &PublicParams<G1, G2, C1, C2>,
  ) -> Result<
    (
      ProverKey<G1, G2, C1, C2, S1, S2>,
      VerifierKey<G1, G2, C1, C2, S1, S2>,
    ),
    NovaError,
  > {
    let (pk_primary, vk_primary) = S1::setup(&pp.ck_primary, &pp.circuit_shape_primary.r1cs_shape)?;
    let (pk_secondary, vk_secondary) =
      S2::setup(&pp.ck_secondary, &pp.circuit_shape_secondary.r1cs_shape)?;

    let pk = ProverKey {
      pk_primary,
      pk_secondary,
      _p: Default::default(),
    };

    let vk = VerifierKey {
      F_arity_primary: pp.F_arity_primary,
      F_arity_secondary: pp.F_arity_secondary,
      ro_consts_primary: pp.ro_consts_primary.clone(),
      ro_consts_secondary: pp.ro_consts_secondary.clone(),
      pp_digest: pp.digest(),
      vk_primary,
      vk_secondary,
      _p: Default::default(),
    };

    Ok((pk, vk))
  }

  /// Create a new `CompressedSNARK`
  pub fn prove(
    pp: &PublicParams<G1, G2, C1, C2>,
    pk: &ProverKey<G1, G2, C1, C2, S1, S2>,
    recursive_snark: &RecursiveSNARK<G1, G2, C1, C2>,
  ) -> Result<Self, NovaError> {
    // fold the secondary circuit's instance with its running instance
    let (nifs_secondary, (f_U_secondary, f_W_secondary)) = NIFS::prove(
      &pp.ck_secondary,
      &pp.ro_consts_secondary,
      &scalar_as_base::<G1>(pp.digest()),
      &pp.circuit_shape_secondary.r1cs_shape,
      &recursive_snark.r_U_secondary,
      &recursive_snark.r_W_secondary,
      &recursive_snark.l_u_secondary,
      &recursive_snark.l_w_secondary,
    )?;

    // create SNARKs proving the knowledge of f_W_primary and f_W_secondary
    let (r_W_snark_primary, f_W_snark_secondary) = rayon::join(
      || {
        S1::prove(
          &pp.ck_primary,
          &pk.pk_primary,
          &pp.circuit_shape_primary.r1cs_shape,
          &recursive_snark.r_U_primary,
          &recursive_snark.r_W_primary,
        )
      },
      || {
        S2::prove(
          &pp.ck_secondary,
          &pk.pk_secondary,
          &pp.circuit_shape_secondary.r1cs_shape,
          &f_U_secondary,
          &f_W_secondary,
        )
      },
    );

    Ok(Self {
      r_U_primary: recursive_snark.r_U_primary.clone(),
      r_W_snark_primary: r_W_snark_primary?,

      r_U_secondary: recursive_snark.r_U_secondary.clone(),
      l_u_secondary: recursive_snark.l_u_secondary.clone(),
      nifs_secondary,
      f_W_snark_secondary: f_W_snark_secondary?,

      zn_primary: recursive_snark.zi_primary.clone(),
      zn_secondary: recursive_snark.zi_secondary.clone(),

      _p: Default::default(),
    })
  }

  /// Verify the correctness of the `CompressedSNARK`
  pub fn verify(
    &self,
    vk: &VerifierKey<G1, G2, C1, C2, S1, S2>,
    num_steps: usize,
    z0_primary: &[G1::Scalar],
    z0_secondary: &[G2::Scalar],
  ) -> Result<(Vec<G1::Scalar>, Vec<G2::Scalar>), NovaError> {
    // the number of steps cannot be zero
    if num_steps == 0 {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the (relaxed) R1CS instances have two public outputs
    if self.l_u_secondary.one_and_X.len() != 3
      || self.l_u_secondary.one_and_X[0] != G2::Scalar::ONE
      || self.r_U_primary.u_and_X.len() != 3
      || self.r_U_secondary.u_and_X.len() != 3
    {
      return Err(NovaError::ProofVerifyError);
    }

    // check if the output hashes in R1CS instances point to the right running instances
    let (hash_primary, hash_secondary) = {
      let mut hasher = <<G2 as Group>::RO as ROTrait<G2::Base, G2::Scalar>>::new(
        vk.ro_consts_secondary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_primary,
      );
      hasher.absorb(vk.pp_digest);
      hasher.absorb(G1::Scalar::from(num_steps as u64));
      for e in z0_primary {
        hasher.absorb(*e);
      }
      for e in &self.zn_primary {
        hasher.absorb(*e);
      }
      self.r_U_secondary.absorb_in_ro(&mut hasher);

      let mut hasher2 = <<G1 as Group>::RO as ROTrait<G1::Base, G1::Scalar>>::new(
        vk.ro_consts_primary.clone(),
        NUM_FE_WITHOUT_IO_FOR_CRHF + 2 * vk.F_arity_secondary,
      );
      hasher2.absorb(scalar_as_base::<G1>(vk.pp_digest));
      hasher2.absorb(G2::Scalar::from(num_steps as u64));
      for e in z0_secondary {
        hasher2.absorb(*e);
      }
      for e in &self.zn_secondary {
        hasher2.absorb(*e);
      }
      self.r_U_primary.absorb_in_ro(&mut hasher2);

      (
        hasher.squeeze(NUM_HASH_BITS),
        hasher2.squeeze(NUM_HASH_BITS),
      )
    };

    if hash_primary != self.l_u_secondary.one_and_X[1]
      || hash_secondary != scalar_as_base::<G2>(self.l_u_secondary.one_and_X[2])
    {
      return Err(NovaError::ProofVerifyError);
    }

    // fold the secondary's running instance with the last instance to get a folded instance
    let f_U_secondary = self.nifs_secondary.verify(
      &vk.ro_consts_secondary,
      &scalar_as_base::<G1>(vk.pp_digest),
      &self.r_U_secondary,
      &self.l_u_secondary,
    )?;

    // check the satisfiability of the folded instances using
    // SNARKs proving the knowledge of their satisfying witnesses
    let (res_primary, res_secondary) = rayon::join(
      || {
        self
          .r_W_snark_primary
          .verify(&vk.vk_primary, &self.r_U_primary)
      },
      || {
        self
          .f_W_snark_secondary
          .verify(&vk.vk_secondary, &f_U_secondary)
      },
    );

    res_primary?;
    res_secondary?;

    Ok((self.zn_primary.clone(), self.zn_secondary.clone()))
  }
}

/// Compute the circuit digest of a [StepCircuit].
///
/// Note for callers: This function should be called with its performance characteristics in mind.
/// It will synthesize and digest the full `circuit` given.
pub fn circuit_digest<
  G1: Group<Base = <G2 as Group>::Scalar>,
  G2: Group<Base = <G1 as Group>::Scalar>,
  C: StepCircuit<G1::Scalar>,
>(
  circuit: &C,
) -> G1::Scalar {
  let augmented_circuit_params = NovaAugmentedCircuitParams::new(BN_LIMB_WIDTH, BN_N_LIMBS, true);

  // ro_consts_circuit are parameterized by G2 because the type alias uses G2::Base = G1::Scalar
  let ro_consts_circuit: ROConstantsCircuit<G2> = ROConstantsCircuit::<G2>::default();

  // Initialize ck for the primary
  let augmented_circuit: NovaAugmentedCircuit<'_, G2, C> =
    NovaAugmentedCircuit::new(&augmented_circuit_params, None, circuit, ro_consts_circuit);
  let mut cs: ShapeCS<G1> = ShapeCS::new();
  let _ = augmented_circuit.synthesize(&mut cs);
  cs.r1cs_shape().digest()
}

type CommitmentKey<G> = <<G as Group>::CE as CommitmentEngineTrait<G>>::CommitmentKey;
type Commitment<G> = <<G as Group>::CE as CommitmentEngineTrait<G>>::Commitment;
type CompressedCommitment<G> = <<<G as Group>::CE as CommitmentEngineTrait<G>>::Commitment as CommitmentTrait<G>>::CompressedCommitment;
type CE<G> = <G as Group>::CE;

#[cfg(test)]
mod tests {
  use crate::provider::bn256_grumpkin::{bn256, grumpkin};
  use crate::provider::secp_secq::{secp256k1, secq256k1};
  use crate::traits::evaluation::EvaluationEngineTrait;
  use crate::traits::snark::default_ck_hint;
  use core::fmt::Write;

  use super::*;
  type EE<G> = provider::ipa_pc::EvaluationEngine<G>;
  type S<G, EE> = spartan::snark::RelaxedR1CSSNARK<G, EE>;
  type SPrime<G, EE> = spartan::ppsnark::RelaxedR1CSSNARK<G, EE>;

  use ::bellpepper_core::{num::AllocatedNum, ConstraintSystem, SynthesisError};
  use core::marker::PhantomData;
  use ff::PrimeField;
  use traits::circuit::TrivialCircuit;

  #[derive(Clone, Debug, Default)]
  struct CubicCircuit<F: PrimeField> {
    _p: PhantomData<F>,
  }

  impl<F> StepCircuit<F> for CubicCircuit<F>
  where
    F: PrimeField,
  {
    fn arity(&self) -> usize {
      1
    }

    fn synthesize<CS: ConstraintSystem<F>>(
      &self,
      cs: &mut CS,
      z: &[AllocatedNum<F>],
    ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
      // Consider a cubic equation: `x^3 + x + 5 = y`, where `x` and `y` are respectively the input and output.
      let x = &z[0];
      let x_sq = x.square(cs.namespace(|| "x_sq"))?;
      let x_cu = x_sq.mul(cs.namespace(|| "x_cu"), x)?;
      let y = AllocatedNum::alloc(cs.namespace(|| "y"), || {
        Ok(x_cu.get_value().unwrap() + x.get_value().unwrap() + F::from(5u64))
      })?;

      cs.enforce(
        || "y = x^3 + x + 5",
        |lc| {
          lc + x_cu.get_variable()
            + x.get_variable()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
            + CS::one()
        },
        |lc| lc + CS::one(),
        |lc| lc + y.get_variable(),
      );

      Ok(vec![y])
    }
  }

  impl<F> CubicCircuit<F>
  where
    F: PrimeField,
  {
    fn output(&self, z: &[F]) -> Vec<F> {
      vec![z[0] * z[0] * z[0] + z[0] + F::from(5u64)]
    }
  }

  fn test_pp_digest_with<G1, G2, T1, T2, E1, E2>(circuit1: &T1, circuit2: &T2, _expected: &str)
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    T1: StepCircuit<G1::Scalar>,
    T2: StepCircuit<G2::Scalar>,
    E1: EvaluationEngineTrait<G1>,
    E2: EvaluationEngineTrait<G2>,
    <G1::Scalar as PrimeField>::Repr: Abomonation,
    <G2::Scalar as PrimeField>::Repr: Abomonation,
  {
    // this tests public parameters with a size specifically intended for a spark-compressed SNARK
    let ck_hint1 = &*SPrime::<G1, E1>::ck_floor();
    let ck_hint2 = &*SPrime::<G2, E2>::ck_floor();
    let pp = PublicParams::<G1, G2, T1, T2>::new(circuit1, circuit2, ck_hint1, ck_hint2);

    let digest_str = pp
      .digest()
      .to_repr()
      .as_ref()
      .iter()
      .fold(String::new(), |mut output, b| {
        let _ = write!(output, "{b:02x}");
        output
      });
    println!("{:?}", digest_str);
    // assert_eq!(digest_str, expected);
  }

  #[test]
  fn test_pp_digest() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;
    let trivial_circuit1 = TrivialCircuit::<<G1 as Group>::Scalar>::default();
    let trivial_circuit2 = TrivialCircuit::<<G2 as Group>::Scalar>::default();
    let cubic_circuit1 = CubicCircuit::<<G1 as Group>::Scalar>::default();

    test_pp_digest_with::<G1, G2, _, _, EE<_>, EE<_>>(
      &trivial_circuit1,
      &trivial_circuit2,
      "cb581e2d5c4b2ef2ddbe2d6849e0da810352f59bcdaca51476dcf9e16072f100",
    );

    test_pp_digest_with::<G1, G2, _, _, EE<_>, EE<_>>(
      &cubic_circuit1,
      &trivial_circuit2,
      "3cc29bb864910463e0501bac84cdefc1d4327e9c2ef5b0fd6d45ad1741f1a401",
    );

    let trivial_circuit1_grumpkin = TrivialCircuit::<<bn256::Point as Group>::Scalar>::default();
    let trivial_circuit2_grumpkin = TrivialCircuit::<<grumpkin::Point as Group>::Scalar>::default();
    let cubic_circuit1_grumpkin = CubicCircuit::<<bn256::Point as Group>::Scalar>::default();

    test_pp_digest_with::<bn256::Point, grumpkin::Point, _, _, EE<_>, EE<_>>(
      &trivial_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "c26cc841d42c19bf98bc2482e66cd30903922f2a923927b85d66f375a821f101",
    );
    test_pp_digest_with::<bn256::Point, grumpkin::Point, _, _, EE<_>, EE<_>>(
      &cubic_circuit1_grumpkin,
      &trivial_circuit2_grumpkin,
      "4c484cab71e93dda69b420beb7276af969c2034a7ffb0ea8e6964e96a7e5a901",
    );

    let trivial_circuit1_secp = TrivialCircuit::<<secp256k1::Point as Group>::Scalar>::default();
    let trivial_circuit2_secp = TrivialCircuit::<<secq256k1::Point as Group>::Scalar>::default();
    let cubic_circuit1_secp = CubicCircuit::<<secp256k1::Point as Group>::Scalar>::default();

    test_pp_digest_with::<secp256k1::Point, secq256k1::Point, _, _, EE<_>, EE<_>>(
      &trivial_circuit1_secp,
      &trivial_circuit2_secp,
      "b794d655fb39891eaf530ca3be1ec2a5ac97f72a0d07c45dbb84529d8a611502",
    );
    test_pp_digest_with::<secp256k1::Point, secq256k1::Point, _, _, EE<_>, EE<_>>(
      &cubic_circuit1_secp,
      &trivial_circuit2_secp,
      "50e6acf363c31c2ac1c9c646b4494cb21aae6cb648c7b0d4c95015c811fba302",
    );
  }

  fn test_ivc_trivial_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let test_circuit1 = TrivialCircuit::<<G1 as Group>::Scalar>::default();
    let test_circuit2 = TrivialCircuit::<<G2 as Group>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      TrivialCircuit<<G2 as Group>::Scalar>,
    >::new(
      &test_circuit1,
      &test_circuit2,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );
    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      &[<G1 as Group>::Scalar::ZERO],
      &[<G2 as Group>::Scalar::ZERO],
    )
    .unwrap();

    let res = recursive_snark.prove_step(&pp, &test_circuit1, &test_circuit2);

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ZERO],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_trivial() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_trivial_with::<G1, G2>();
    test_ivc_trivial_with::<bn256::Point, grumpkin::Point>();
    test_ivc_trivial_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nontrivial_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    )
    .unwrap();

    for i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());

      // verify the recursive snark at each step of recursion
      let res = recursive_snark.verify(
        &pp,
        i + 1,
        &[<G1 as Group>::Scalar::ONE],
        &[<G2 as Group>::Scalar::ZERO],
      );
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);
  }

  #[test]
  fn test_ivc_nontrivial() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with::<G1, G2>();
    test_ivc_nontrivial_with::<bn256::Point, grumpkin::Point>();
    test_ivc_nontrivial_with::<secp256k1::Point, secq256k1::Point>();
  }

  fn test_ivc_nontrivial_with_compression_with<G1, G2, E1, E2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    E1: EvaluationEngineTrait<G1>,
    E2: EvaluationEngineTrait<G2>,
    // this is due to the reliance on Abomonation
    <<G1 as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2 as Group>::Scalar as PrimeField>::Repr: Abomonation,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    )
    .unwrap();

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = circuit_secondary.clone().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S<G1, E1>, S<G2, E2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res =
      CompressedSNARK::<_, _, _, _, S<G1, E1>, S<G2, E2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with_compression_with::<G1, G2, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_compression_with::<bn256::Point, grumpkin::Point, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_compression_with::<secp256k1::Point, secq256k1::Point, EE<_>, EE<_>>();
  }

  fn test_ivc_nontrivial_with_spark_compression_with<G1, G2, E1, E2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    E1: EvaluationEngineTrait<G1>,
    E2: EvaluationEngineTrait<G2>,
    // this is due to the reliance on Abomonation
    <<G1 as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2 as Group>::Scalar as PrimeField>::Repr: Abomonation,
  {
    let circuit_primary = TrivialCircuit::default();
    let circuit_secondary = CubicCircuit::default();

    // produce public parameters, which we'll use with a spark-compressed SNARK
    let pp = PublicParams::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &circuit_primary,
      &circuit_secondary,
      &*SPrime::<_, E1>::ck_floor(),
      &*SPrime::<_, E2>::ck_floor(),
    );

    let num_steps = 3;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &circuit_primary,
      &circuit_secondary,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    )
    .unwrap();

    for _i in 0..num_steps {
      let res = recursive_snark.prove_step(&pp, &circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    // sanity: check the claimed output with a direct computation of the same
    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    let mut zn_secondary_direct = vec![<G2 as Group>::Scalar::ZERO];
    for _i in 0..num_steps {
      zn_secondary_direct = CubicCircuit::default().output(&zn_secondary_direct);
    }
    assert_eq!(zn_secondary, zn_secondary_direct);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(2460515u64)]);

    // run the compressed snark with Spark compiler
    // produce the prover and verifier keys for compressed snark
    let (pk, vk) =
      CompressedSNARK::<_, _, _, _, SPrime<G1, E1>, SPrime<G2, E2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res = CompressedSNARK::<_, _, _, _, SPrime<G1, E1>, SPrime<G2, E2>>::prove(
      &pp,
      &pk,
      &recursive_snark,
    );
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(
      &vk,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nontrivial_with_spark_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nontrivial_with_spark_compression_with::<G1, G2, EE<_>, EE<_>>();
    test_ivc_nontrivial_with_spark_compression_with::<bn256::Point, grumpkin::Point, EE<_>, EE<_>>(
    );
    test_ivc_nontrivial_with_spark_compression_with::<
      secp256k1::Point,
      secq256k1::Point,
      EE<_>,
      EE<_>,
    >();
  }

  fn test_ivc_nondet_with_compression_with<G1, G2, E1, E2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
    E1: EvaluationEngineTrait<G1>,
    E2: EvaluationEngineTrait<G2>,
    // this is due to the reliance on Abomonation
    <<G1 as Group>::Scalar as PrimeField>::Repr: Abomonation,
    <<G2 as Group>::Scalar as PrimeField>::Repr: Abomonation,
  {
    // y is a non-deterministic advice representing the fifth root of the input at a step.
    #[derive(Clone, Debug)]
    struct FifthRootCheckingCircuit<F: PrimeField> {
      y: F,
    }

    impl<F> FifthRootCheckingCircuit<F>
    where
      F: PrimeField,
    {
      fn new(num_steps: usize) -> (Vec<F>, Vec<Self>) {
        let mut powers = Vec::new();
        let rng = &mut rand::rngs::OsRng;
        let mut seed = F::random(rng);
        for _i in 0..num_steps + 1 {
          seed *= seed.clone().square().square();

          powers.push(Self { y: seed });
        }

        // reverse the powers to get roots
        let roots = powers.into_iter().rev().collect::<Vec<Self>>();
        (vec![roots[0].y], roots[1..].to_vec())
      }
    }

    impl<F> StepCircuit<F> for FifthRootCheckingCircuit<F>
    where
      F: PrimeField,
    {
      fn arity(&self) -> usize {
        1
      }

      fn synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
      ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
        let x = &z[0];

        // we allocate a variable and set it to the provided non-deterministic advice.
        let y = AllocatedNum::alloc_infallible(cs.namespace(|| "y"), || self.y);

        // We now check if y = x^{1/5} by checking if y^5 = x
        let y_sq = y.square(cs.namespace(|| "y_sq"))?;
        let y_quad = y_sq.square(cs.namespace(|| "y_quad"))?;
        let y_pow_5 = y_quad.mul(cs.namespace(|| "y_fifth"), &y)?;

        cs.enforce(
          || "y^5 = x",
          |lc| lc + y_pow_5.get_variable(),
          |lc| lc + CS::one(),
          |lc| lc + x.get_variable(),
        );

        Ok(vec![y])
      }
    }

    let circuit_primary = FifthRootCheckingCircuit {
      y: <G1 as Group>::Scalar::ZERO,
    };

    let circuit_secondary = TrivialCircuit::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialCircuit<<G2 as Group>::Scalar>,
    >::new(
      &circuit_primary,
      &circuit_secondary,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 3;

    // produce non-deterministic advice
    let (z0_primary, roots) = FifthRootCheckingCircuit::new(num_steps);
    let z0_secondary = vec![<G2 as Group>::Scalar::ZERO];

    // produce a recursive SNARK
    let mut recursive_snark: RecursiveSNARK<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialCircuit<<G2 as Group>::Scalar>,
    > = RecursiveSNARK::<
      G1,
      G2,
      FifthRootCheckingCircuit<<G1 as Group>::Scalar>,
      TrivialCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &roots[0],
      &circuit_secondary,
      &z0_primary,
      &z0_secondary,
    )
    .unwrap();

    for circuit_primary in roots.iter().take(num_steps) {
      let res = recursive_snark.prove_step(&pp, circuit_primary, &circuit_secondary);
      assert!(res.is_ok());
    }

    // verify the recursive SNARK
    let res = recursive_snark.verify(&pp, num_steps, &z0_primary, &z0_secondary);
    assert!(res.is_ok());

    // produce the prover and verifier keys for compressed snark
    let (pk, vk) = CompressedSNARK::<_, _, _, _, S<G1, E1>, S<G2, E2>>::setup(&pp).unwrap();

    // produce a compressed SNARK
    let res =
      CompressedSNARK::<_, _, _, _, S<G1, E1>, S<G2, E2>>::prove(&pp, &pk, &recursive_snark);
    assert!(res.is_ok());
    let compressed_snark = res.unwrap();

    // verify the compressed SNARK
    let res = compressed_snark.verify(&vk, num_steps, &z0_primary, &z0_secondary);
    assert!(res.is_ok());
  }

  #[test]
  fn test_ivc_nondet_with_compression() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_nondet_with_compression_with::<G1, G2, EE<_>, EE<_>>();
    test_ivc_nondet_with_compression_with::<bn256::Point, grumpkin::Point, EE<_>, EE<_>>();
    test_ivc_nondet_with_compression_with::<secp256k1::Point, secq256k1::Point, EE<_>, EE<_>>();
  }

  fn test_ivc_base_with<G1, G2>()
  where
    G1: Group<Base = <G2 as Group>::Scalar>,
    G2: Group<Base = <G1 as Group>::Scalar>,
  {
    let test_circuit1 = TrivialCircuit::<<G1 as Group>::Scalar>::default();
    let test_circuit2 = CubicCircuit::<<G2 as Group>::Scalar>::default();

    // produce public parameters
    let pp = PublicParams::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &test_circuit1,
      &test_circuit2,
      &*default_ck_hint(),
      &*default_ck_hint(),
    );

    let num_steps = 1;

    // produce a recursive SNARK
    let mut recursive_snark = RecursiveSNARK::<
      G1,
      G2,
      TrivialCircuit<<G1 as Group>::Scalar>,
      CubicCircuit<<G2 as Group>::Scalar>,
    >::new(
      &pp,
      &test_circuit1,
      &test_circuit2,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    )
    .unwrap();

    // produce a recursive SNARK
    let res = recursive_snark.prove_step(&pp, &test_circuit1, &test_circuit2);

    assert!(res.is_ok());

    // verify the recursive SNARK
    let res = recursive_snark.verify(
      &pp,
      num_steps,
      &[<G1 as Group>::Scalar::ONE],
      &[<G2 as Group>::Scalar::ZERO],
    );
    assert!(res.is_ok());

    let (zn_primary, zn_secondary) = res.unwrap();

    assert_eq!(zn_primary, vec![<G1 as Group>::Scalar::ONE]);
    assert_eq!(zn_secondary, vec![<G2 as Group>::Scalar::from(5u64)]);
  }

  #[test]
  fn test_ivc_base() {
    type G1 = pasta_curves::pallas::Point;
    type G2 = pasta_curves::vesta::Point;

    test_ivc_base_with::<G1, G2>();
    test_ivc_base_with::<bn256::Point, grumpkin::Point>();
    test_ivc_base_with::<secp256k1::Point, secq256k1::Point>();
  }
}

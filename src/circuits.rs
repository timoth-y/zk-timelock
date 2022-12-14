use crate::poseidon::get_poseidon_params;
use anyhow::anyhow;
use ark_ff::{BigInteger, BitIteratorLE, Field, PrimeField, ToConstraintField, Zero, Fp12, One, QuadExtField, BigInteger384, Fp12ConfigWrapper};
use ark_r1cs_std::fields::fp::FpVar;
use ark_r1cs_std::prelude::*;
use ark_r1cs_std::ToConstraintFieldGadget;
use ark_relations::ns;
use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystem, ConstraintSystemRef, Namespace, SynthesisError};
use ark_r1cs_std::groups::{bls12, CurveVar};
use ark_snark::{CircuitSpecificSetupSNARK, SNARK};
use ark_sponge::poseidon::constraints::PoseidonSpongeVar;
use ark_sponge::poseidon::{PoseidonConfig, PoseidonSponge};
use ark_std::marker::PhantomData;
use ark_std::rand::{CryptoRng, Rng, RngCore};
use ark_std::vec::Vec;
use ark_std::UniformRand;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::hash::Hash;
use std::ops::{Add, Mul, MulAssign};
use std::str::FromStr;
use ark_ec::bls12::Bls12Parameters;
use ark_r1cs_std::fields::fp12::Fp12Var;
use ark_ec::{pairing::Pairing, CurveGroup, Group};
use ark_sponge::constraints::CryptographicSpongeVar;
use ark_sponge::{Absorb, CryptographicSponge, FieldBasedCryptographicSponge};
use group::Curve as _;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_bls12_381::Bls12_381;
use ark_r1cs_std::fields::nonnative::NonNativeFieldVar;
use ark_r1cs_std::groups::curves::short_weierstrass::ProjectiveVar;
use crate::utils::{curve_scalar_mul_le, gt_scalar_mul_le, GtAbsorbable, gtvar_to_fqvars, Hash2Curve, ZkCryptoDeserialize};
use sha2::Sha256;
use crate::nonnative::*;
use crate::{Randomness, Plaintext, Ciphertext, PublicKey, SecretKey, Parameters};

const R_BYTES_SQUEEZE: usize = 32;
const H2C_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

pub struct Circuit<E: Pairing, P: Bls12Parameters<Fp = <E::G1 as CurveGroup>::BaseField>>
    where <E::G1 as CurveGroup>::BaseField: PrimeField
{
    sigma: Randomness<E::G1>,
    master: PublicKey<E>,
    msg: Plaintext<E::G1>,
    pub ciphertext: Ciphertext<E::G1>,
    pub gid: E::TargetField,
    params: Parameters<E::G1>,
    _curve_params: PhantomData<P>
}

impl<E: Pairing, P: Bls12Parameters<Fp = <E::G1 as CurveGroup>::BaseField>> Circuit<E, P>
    where <E::G1 as CurveGroup>::BaseField: PrimeField + Absorb,
          E: Hash2Curve + GtAbsorbable,
          ProjectiveVar<P::G1Parameters, FpVar<P::Fp>>: AllocVar<E::G1, <E::G1 as CurveGroup>::BaseField> + CurveVar<E::G1, <E::G1 as CurveGroup>::BaseField> + AllocVar<E::G1, P::Fp>,
{
    type Fq = <E::G1 as CurveGroup>::BaseField;

    pub fn new<I: AsRef<[u8]>, R: Rng>(
        master: PublicKey<E>,
        id: I,
        msg: Plaintext<E::G1>,
        rng: &mut R,
    ) -> anyhow::Result<Self> {
        let params = Parameters::<E::G1>::default();

        let (gid, sigma, ct) = Self::encrypt_inner(&master, id, &msg, &params, rng)
            .map_err(|e| anyhow!("error encrypting message: {e}"))?;

        Ok(Self {
            gid,
            sigma,
            msg,
            master,
            ciphertext: ct,
            params,
            _curve_params: Default::default()
        })
    }

    pub fn encrypt<I: AsRef<[u8]>, R: Rng>(
        master: &PublicKey<E>,
        id: I,
        msg: &Plaintext<E::G1>,
        rng: &mut R,
    ) -> anyhow::Result<Ciphertext<E::G1>> {
        let params = Parameters::<E::G1>::default();
        let (_, _, ct) = Self::encrypt_inner(master, id, msg, &params, rng)?;
        Ok(ct)
    }

    fn encrypt_inner<I: AsRef<[u8]>, R: Rng>(
        master: &PublicKey<E>,
        id: I,
        msg: &Plaintext<E::G1>,
        params: &Parameters<E::G1>,
        rng: &mut R,
    ) -> anyhow::Result<(E::TargetField, Randomness<E::G1>, Ciphertext<E::G1>)> {
        // 1. Compute Gid = e(master,Q_id)
        // Note: hash-to-curve algo is `draft-irtf-cfrg-bls-signature-05` which matches to the one used in Drand network,
        // hash function is Sha2 despite the fact that poseidon is used elsewhere to optimize proving performance.
        let gid = {
            let qid: E::G2Affine = E::hash(id.as_ref(), H2C_DST)?;
            E::pairing(master.clone(), qid)
        }.0;

        // 2. Derive random sigma
        let sigma = Randomness::<E::G1>::rand(rng);

        // 3. Derive r from sigma and msg
        let r = {
            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&sigma.0);
            sponge.absorb(&msg);
            sponge.squeeze_bytes(R_BYTES_SQUEEZE)
        };

        // 4. Compute U = G*r
        let mut u = curve_scalar_mul_le(
            E::G1::generator(),
            &r
        );

        // 5. Compute V = sigma XOR H(rGid)
        let v = {
            let mut r_gid: E::TargetField = gt_scalar_mul_le(gid.clone(), &r);

            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&E::gt_to_absorbable(&r_gid));
            let h_r_gid = sponge.squeeze_native_field_elements(1).remove(0);
            sigma.0 + h_r_gid
        };

        // 6. Compute W = M XOR H(sigma)
        let w = {
            // todo: could we skip hashing here?
            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&sigma.0);
            let h_sigma = sponge.squeeze_native_field_elements(1).remove(0);
            (*msg).clone() + h_sigma
        };

        Ok((gid, sigma, Ciphertext{
            u,
            v,
            w
        }))
    }

    #[inline]
    pub fn decrypt(
        sk: &SecretKey<E>,
        ct: &Ciphertext<E::G1>,
    ) -> anyhow::Result<Plaintext<E::G1>> {
        let params = Parameters::<E::G1>::default();

        // 1. Compute sigma = V XOR H2(e(rP,private))
        let sigma = {
            let r_gid = E::pairing(ct.u.clone(), sk.clone()).0;

            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&E::gt_to_absorbable(&r_gid));
            let h_r_gid = sponge.squeeze_native_field_elements(1).remove(0);

            ct.v - h_r_gid
        };

        // 2. Compute Msg = W XOR H4(sigma)
        let msg = {
            // todo: could we skip hashing here?
            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&sigma);
            let h_sigma = sponge.squeeze_native_field_elements(1).remove(0);
            ct.w.clone() - h_sigma
        };

        // 3. Check U = G^r
        let r_g = {
            let mut sponge = PoseidonSponge::new(&params.poseidon);
            sponge.absorb(&sigma);
            sponge.absorb(&msg);
            let r = sponge.squeeze_bytes(R_BYTES_SQUEEZE);
            curve_scalar_mul_le(E::G1::generator(), &r)
        };
        assert_eq!(ct.u, r_g);

        Ok(msg)
    }

    pub fn get_public_inputs(
        gid: &E::TargetField,
        cipher: &Ciphertext<E::G1>,
    ) -> Vec<<E::G1 as CurveGroup>::BaseField>
        where
            E::G1: ToConstraintField<<E::G1 as CurveGroup>::BaseField>,
            E::TargetField: ToConstraintField<<E::G1 as CurveGroup>::BaseField>,
    {
        let gid_inputs = gid.to_field_elements().unwrap();

        let mut u_inputs = cipher.u.to_field_elements().unwrap();
        let v_inputs = cipher.v.to_field_elements().unwrap();
        let w_inputs = cipher.w.to_field_elements().unwrap();

        // Fix for the different behavior of Short Weierstrass `G1Var::new_input` and `G1::to_field_elements`.
        // See: https://github.com/arkworks-rs/r1cs-std/issues/106
        u_inputs[2] = <E::G1 as CurveGroup>::BaseField::one();

        gid_inputs.into_iter().chain(u_inputs).chain(v_inputs).chain(w_inputs).collect()
    }

    pub(crate) fn verify_encryption(
        &self,
        cs: ConstraintSystemRef<<E::G1 as CurveGroup>::BaseField>,
        gid: Fp12Var<P::Fp12Config>,
        msg: &FpVar<<E::G1 as CurveGroup>::BaseField>,
        ct: &(bls12::G1Var<P>, FpVar<<E::G1 as CurveGroup>::BaseField>, FpVar<<E::G1 as CurveGroup>::BaseField>),
    ) -> Result<(), SynthesisError> {
        // 2. Derive random sigma
        let sigma = FpVar::<<E::G1 as CurveGroup>::BaseField>::new_witness(ns!(cs, "sigma"), || Ok(&self.sigma.0))?;

        // 3. Derive r from sigma and msg
        let r = {
            let mut sponge = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            sponge.absorb(&sigma)?;
            sponge.absorb(&msg)?;
            sponge
                .squeeze_bytes(R_BYTES_SQUEEZE)?
                .into_iter().flat_map(|byte| byte.to_bits_le().unwrap()).collect::<Vec<_>>()
        };

        // 4. Compute U = G*r
        let g = bls12::G1Var::<P>::new_constant(ns!(cs, "generator"), E::G1::generator())?;
        let u = g.scalar_mul_le(r.iter())?;
        u.enforce_equal(&ct.0)?;

        // 5. Compute V = sigma XOR H(rGid)
        let v = {
            let r_gid = {
                let mut res = Fp12Var::<P::Fp12Config>::one();
                let mut mul = gid;
                for bit in r.into_iter() {
                    let tmp = res.clone() * &mul;
                    res = bit.select(&tmp, &res)?;
                    mul.square_in_place()?;
                }
                res
            };
            let mut sponge = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            sponge.absorb(&gtvar_to_fqvars::<E, P>(&r_gid))?;

            let h_r_gid = sponge
                .squeeze_field_elements(1)
                .and_then(|r| Ok(r[0].clone()))?;

            &sigma + h_r_gid
        };
        v.enforce_equal(&ct.1)?;


        // 6. Compute W = M XOR H(sigma)
        let w = {
            let mut poseidon = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            poseidon.absorb(&sigma)?;
            let h_sigma = poseidon
                .squeeze_field_elements(1)
                .and_then(|r| Ok(r[0].clone()))?;

            msg + h_sigma
        };
        w.enforce_equal(&ct.2)?;

        Ok(())
    }

    pub(crate) fn ciphertext_var(
        &self,
        cs: ConstraintSystemRef<<E::G1 as CurveGroup>::BaseField>,
        mode: AllocationMode,
    ) -> Result<(bls12::G1Var<P>, FpVar<<E::G1 as CurveGroup>::BaseField>, FpVar<<E::G1 as CurveGroup>::BaseField>), SynthesisError> {
        let u = bls12::G1Var::<P>::new_variable(
            ns!(cs, "ciphertext_u"),
            || Ok(self.ciphertext.u),
            mode,
        )?;

        let v = FpVar::<<E::G1 as CurveGroup>::BaseField>::new_variable(
            ns!(cs, "ciphertext_v"),
            || {
                Ok(self.ciphertext.v)
            },
            mode,
        )?;

        let w = FpVar::<<E::G1 as CurveGroup>::BaseField>::new_variable(
            ns!(cs, "ciphertext_w"),
            || {
                Ok(self.ciphertext.w)
            },
            mode,
        )?;

        Ok((u, v, w))
    }
}

impl<E: Pairing, P: Bls12Parameters<Fp = <E::G1 as CurveGroup>::BaseField>> ConstraintSynthesizer<<E::G1 as CurveGroup>::BaseField> for Circuit<E, P>
    where <E::G1 as CurveGroup>::BaseField: PrimeField + Absorb,
          E: Hash2Curve + GtAbsorbable,
          E::TargetField: Borrow<QuadExtField<Fp12ConfigWrapper<<P as Bls12Parameters>::Fp12Config>>>,
          ProjectiveVar<P::G1Parameters, FpVar<P::Fp>>: AllocVar<E::G1, <E::G1 as CurveGroup>::BaseField> + CurveVar<E::G1, <E::G1 as CurveGroup>::BaseField> + AllocVar<E::G1, P::Fp>,
{
    fn generate_constraints(
        self,
        cs: ConstraintSystemRef<<E::G1 as CurveGroup>::BaseField>,
    ) -> Result<(), SynthesisError> {
        let gid = Fp12Var::<P::Fp12Config>::new_input(ns!(cs, "gid"), || Ok(self.gid))?;
        let ciphertext = self.ciphertext_var(cs.clone(), AllocationMode::Input)?;
        let message = FpVar::<<E::G1 as CurveGroup>::BaseField>::new_witness(ns!(cs, "plaintext"), || {
            Ok(self.msg)
        })?;

        self.verify_encryption(cs.clone(), gid, &message, &ciphertext)
    }
}

pub struct NonnativeCircuit<PC: CurveGroup>
    where PC::BaseField: PrimeField
{
    gid: ark_bls12_381::Fq12,
    sigma: Randomness<ark_bls12_381::G1Projective>,
    master: PublicKey<Bls12_381>,
    msg: Plaintext<ark_bls12_381::G1Projective>,
    pub ciphertext: Ciphertext<ark_bls12_381::G1Projective>,
    params: Parameters<PC>,
}

impl<PC: CurveGroup> NonnativeCircuit<PC>
    where PC::BaseField: PrimeField
{
    pub fn new<I: AsRef<[u8]>, R: Rng>(
        master: PublicKey<Bls12_381>,
        id: I,
        msg: Plaintext<ark_bls12_381::G1Projective>,
        rng: &mut R,
    ) -> anyhow::Result<Self> {
        let pp_381 = Parameters::<ark_bls12_381::G1Projective>::default();
        let params = Parameters::<PC>::default();

        let (gid, sigma, ct) = Circuit::<Bls12_381, ark_bls12_381::Parameters>::encrypt_inner(&master, id, &msg, &pp_381, rng)
            .map_err(|e| anyhow!("error encrypting message: {e}"))?;

        Ok(Self {
            gid,
            sigma,
            msg,
            master,
            ciphertext: ct,
            params,
        })
    }

    pub fn decrypt(
        sk: &SecretKey<Bls12_381>,
        ct: &Ciphertext<ark_bls12_381::G1Projective>,
    ) -> anyhow::Result<Plaintext<ark_bls12_381::G1Projective>> {
        Circuit::<Bls12_381, ark_bls12_381::Parameters>::decrypt(sk, ct)
    }

    // pub fn get_public_input(
    //     gid: &ark_bls12_381::Fq12,
    //     cipher: &Ciphertext<ark_bls12_381::G1Projective>,
    // ) -> Vec<PC::BaseField>
    // {
    //     let gid_inputs = gid.to_field_elements().unwrap();
    //
    //     let mut u_inputs = cipher.u.to_field_elements().unwrap();
    //     let v_inputs = cipher.v.to_field_elements().unwrap();
    //     let w_inputs = cipher.w.to_field_elements().unwrap();
    //
    //     u_inputs[2] = ark_bls12_381::Fq::one();
    //
    //     gid_inputs.into_iter().chain(u_inputs).chain(v_inputs).chain(w_inputs).collect()
    // }

    pub(crate) fn verify_encryption(
        &self,
        cs: ConstraintSystemRef<PC::BaseField>,
        gid: Fq12Var<PC::BaseField>,
        msg: &FqVar<PC::BaseField>,
        ct: &(G1Var<PC::BaseField>, FqVar<PC::BaseField>, FqVar<PC::BaseField>),
    ) -> Result<(), SynthesisError> {
        let g_x = FqVar::new_constant(
            ns!(cs, "ciphertext_u_x"),
            &ark_bls12_381::G1Projective::generator().x
        )?;
        let g_y = FqVar::new_constant(
            ns!(cs, "ciphertext_u_x"),
            &ark_bls12_381::G1Projective::generator().y
        )?;
        let g = G1Var::new(
            g_x, g_y
        );

        // 2. Derive random sigma
        let sigma = FqVar::new_witness(ns!(cs, "sigma"), || Ok(&self.sigma.0))?;


        // 3. Derive r from sigma and msg
        let r = {
            let mut sponge = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            sponge.absorb(&sigma.to_constraint_field().unwrap())?;
            sponge.absorb(&msg.to_constraint_field().unwrap())?;
            sponge
                .squeeze_bytes(R_BYTES_SQUEEZE)?
                .into_iter().flat_map(|b| b.to_bits_le().unwrap()).collect::<Vec<_>>()
        };

        // 4. Compute U = G*r
        let u = g.scalar_mul_le(r.iter())?;
        u.enforce_equal(&ct.0)?;

        // 5. Compute V = sigma XOR H(rGid)
        let v = {
            let r_gid = gid.scalar_mul_le(r.iter())?;
            let mut sponge = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            sponge.absorb(&r_gid)?;

            let h_r_gid = sponge
                .squeeze_nonnative_field_elements(1)
                .and_then(|r| Ok(r.0[0].clone()))?;

            &sigma + h_r_gid
        };
        v.enforce_equal(&ct.1)?;


        // 6. Compute W = M XOR H(sigma)
        let w = {
            let mut poseidon = PoseidonSpongeVar::new(cs.clone(), &self.params.poseidon);
            poseidon.absorb(&sigma.to_constraint_field().unwrap())?;
            let h_sigma = poseidon
                .squeeze_nonnative_field_elements::<ark_bls12_381::Fq>(1)
                .and_then(|r| Ok(r.0[0].clone()))?;

            msg + h_sigma
        };

        w.enforce_equal(&ct.2)?;

        Ok(())
    }

    pub(crate) fn ciphertext_var(
        &self,
        cs: ConstraintSystemRef<PC::BaseField>,
        mode: AllocationMode,
    ) -> Result<(G1Var<PC::BaseField>, FqVar<PC::BaseField>, FqVar<PC::BaseField>), SynthesisError> {
        let u_x = FqVar::new_variable(
            ns!(cs, "ciphertext_u_x"),
            || {
                Ok(self.ciphertext.u.x)
            },
            mode,
        )?;
        let u_y = FqVar::new_variable(
            ns!(cs, "ciphertext_u_x"),
            || {
                Ok(self.ciphertext.u.y)
            },
            mode,
        )?;
        let u = G1Var::new(
            u_x, u_y
        );

        let v = FqVar::new_variable(
            ns!(cs, "ciphertext_v"),
            || {
                Ok(self.ciphertext.v)
            },
            mode,
        )?;

        let w = FqVar::new_variable(
            ns!(cs, "ciphertext_w"),
            || {
                Ok(self.ciphertext.w)
            },
            mode,
        )?;

        Ok((u, v, w))
    }
}

impl<PC: CurveGroup> ConstraintSynthesizer<PC::BaseField> for NonnativeCircuit<PC>
    where PC::BaseField: PrimeField
{
    fn generate_constraints(
        self,
        cs: ConstraintSystemRef<PC::BaseField>,
    ) -> Result<(), SynthesisError> {
        let gid = new_fp12_variable::<_, PC::BaseField>(ns!(cs, "gid"), || Ok(self.gid), AllocationMode::Input)?;
        let message = FqVar::new_witness(ns!(cs, "plaintext"), || {
            Ok(self.msg)
        })?;
        let ciphertext = self.ciphertext_var(cs.clone(), AllocationMode::Input)?;

        self.verify_encryption(cs.clone(), gid, &message, &ciphertext)
    }
}

// This is a modified native circuit for experimental use with Gemini proving system.
// Due to mis-configured (hopefully) padding mechanism of Gemini this circuit intentionally has no input variables.
// For more details see: https://github.com/arkworks-rs/gemini/issues/5
pub struct GeminiNativeCircuit(pub Circuit<Bls12_381, ark_bls12_381::Parameters>);

impl ConstraintSynthesizer<ark_bls12_381::Fq> for GeminiNativeCircuit {
    fn generate_constraints(
        self,
        cs: ConstraintSystemRef<ark_bls12_381::Fq>,
    ) -> Result<(), SynthesisError> {
        let gid = Fp12Var::<ark_bls12_381::Fq12Config>::new_witness(ns!(cs, "gid"), || Ok(self.0.gid))?;
        let ciphertext = self.0.ciphertext_var(cs.clone(), AllocationMode::Witness)?;
        let message = FpVar::<ark_bls12_381::Fq>::new_witness(ns!(cs, "plaintext"), || {
            Ok(self.0.msg)
        })?;

        self.0.verify_encryption(cs.clone(), gid, &message, &ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use ark_std::test_rng;
    use crate::poseidon;
    use super::*;

    use ark_bls12_377::{G1Projective as ProjectiveEngine, Fq, Fr, Fq12, G1Affine, Bls12_377};
    use ark_bw6_761::BW6_761;
    use ark_ec::AffineRepr;

    use ark_ff::{Field, Zero};
    // use ark_groth16::Groth16;
    use ark_serialize::CanonicalSerialize;

    use ark_snark::{CircuitSpecificSetupSNARK, SNARK};
    use sha2::Digest;

    #[test]
    fn test_decrypt() {
        type TestCircuit = Circuit::<Bls12_381, ark_bls12_381::Parameters>;
        let mut rng = test_rng();
        let bytes = [1, 2, 3];
        let msg = ark_bls12_381::Fq::from_random_bytes(&bytes).unwrap();

        let pk = {
            let bytes = hex::decode("8200fc249deb0148eb918d6e213980c5d01acd7fc251900d9260136da3b54836ce125172399ddc69c4e3e11429b62c11").unwrap();
            ark_bls12_381::G1Affine::deserialize_zk_crypto(&bytes).unwrap()
        };

        let round_number = 1000u64;
        let id = {
            let mut hash = sha2::Sha256::new();
            hash.update(&round_number.to_be_bytes());
            &hash.finalize().to_vec()[0..32]
        };

        let ct = TestCircuit::encrypt(&pk, id, &msg, &mut rng).unwrap();

        let sk = {
            let bytes = hex::decode("a4721e6c3eafcd823f138cd29c6c82e8c5149101d0bb4bafddbac1c2d1fe3738895e4e21dd4b8b41bf007046440220910bb1cdb91f50a84a0d7f33ff2e8577aa62ac64b35a291a728a9db5ac91e06d1312b48a376138d77b4d6ad27c24221afe").unwrap();
            ark_bls12_381::G2Affine::deserialize_zk_crypto(&bytes).unwrap()
        };

        let pt = TestCircuit::decrypt(&sk, &ct).unwrap();
        assert_eq!(pt, msg)
    }
}

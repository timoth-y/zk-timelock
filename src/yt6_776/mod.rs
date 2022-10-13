//! This module implements the YT6-776 application-specific curve.
//! The name denotes that it was generated using the Cocks--Pinch method for the
//! embedding degree 6. The main feature of this curve is that the scalar field
//! equals the base field of the BLS12-381 curve.
//!
//! The rough estimates give 124-bit security.
//!
//! Curve information:
//! * Base field: q = 302876569457825540224058720088493814197684678175517897646382999490010176693949664027430922002605277999717929660119065492046541203055097398745672542166604177101118255582761412697357085679229754433270902868922720449830309670836412672963
//! * Scalar field: r = 4002409555221667393417789825735904156556882819939007885332058136124031650490837864442687629129015664037894272559787
//! * Trace: t = 556334928175811767685866265168019893274028091673155517508216967661521459911236919644960862098008653606888062617430745
//! * Fundamental discriminant: D = -3
//!
//! Elliptic curve defined by y^2 = x^3 + 93312.

mod curves;
mod fields;

pub use curves::*;
pub use fields::*;
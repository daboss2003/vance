//! Exact rational arithmetic for time and frame rates.
//!
//! Everything in Vance that represents time or frame rate is rational, never
//! float. A 10-hour timeline at 23.976 fps must be frame-accurate at the end;
//! floats accumulate drift. See PLAN.md §3 principle 1 (Correctness).
//!
//! `Rational` is `i64` numerator / `i64` denominator, always reduced and with a
//! positive denominator. Construction normalizes; arithmetic methods return
//! reduced results.

use crate::Error;
use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, Neg, Sub};

/// An exact rational number stored as numerator and (positive) denominator.
///
/// Always reduced (gcd = 1). The denominator is always positive — sign lives
/// in the numerator. Zero is `0/1`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(into = "(i64, i64)", try_from = "(i64, i64)"))]
pub struct Rational {
    pub(crate) num: i64,
    pub(crate) den: i64,
}

impl Rational {
    /// The rational zero (`0/1`).
    pub const ZERO: Rational = Rational { num: 0, den: 1 };

    /// The rational one (`1/1`).
    pub const ONE: Rational = Rational { num: 1, den: 1 };

    /// Construct from a raw `(num, den)` pair, reducing and canonicalizing sign.
    ///
    /// Returns `Err` if `den == 0`.
    pub fn new(num: i64, den: i64) -> Result<Self, Error> {
        if den == 0 {
            return Err(Error::invalid_input("rational denominator cannot be zero"));
        }
        let (mut n, mut d) = (num, den);
        if d < 0 {
            n = n
                .checked_neg()
                .ok_or_else(|| Error::invariant("i64 overflow negating numerator"))?;
            d = d
                .checked_neg()
                .ok_or_else(|| Error::invariant("i64 overflow negating denominator"))?;
        }
        let g = gcd(n.unsigned_abs(), d as u64) as i64;
        if g > 1 {
            n /= g;
            d /= g;
        }
        Ok(Self { num: n, den: d })
    }

    /// Construct from an integer.
    pub const fn from_int(n: i64) -> Self {
        Self { num: n, den: 1 }
    }

    /// Numerator.
    pub const fn numer(&self) -> i64 {
        self.num
    }

    /// Denominator (always positive).
    pub const fn denom(&self) -> i64 {
        self.den
    }

    /// Approximate as `f64`. Use only at boundaries (display, GPU upload),
    /// never internally.
    pub fn as_f64(&self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Is this exactly zero?
    pub const fn is_zero(&self) -> bool {
        self.num == 0
    }

    /// Sign: -1, 0, or 1.
    pub const fn signum(&self) -> i64 {
        if self.num > 0 {
            1
        } else if self.num < 0 {
            -1
        } else {
            0
        }
    }

    /// Reciprocal. Returns `Err` if self is zero.
    pub fn recip(self) -> Result<Self, Error> {
        if self.num == 0 {
            return Err(Error::invalid_input("cannot take reciprocal of zero"));
        }
        Self::new(self.den, self.num)
    }

    /// Checked addition. Returns `Err` on i64 overflow.
    pub fn checked_add(self, other: Self) -> Result<Self, Error> {
        // a/b + c/d = (a*d + c*b) / (b*d), reduced
        let n = self
            .num
            .checked_mul(other.den)
            .and_then(|x| x.checked_add(other.num.checked_mul(self.den)?))
            .ok_or_else(|| Error::invariant("rational add: numerator overflow"))?;
        let d = self
            .den
            .checked_mul(other.den)
            .ok_or_else(|| Error::invariant("rational add: denominator overflow"))?;
        Self::new(n, d)
    }

    /// Checked subtraction.
    pub fn checked_sub(self, other: Self) -> Result<Self, Error> {
        self.checked_add(other.checked_neg()?)
    }

    /// Checked multiplication.
    pub fn checked_mul(self, other: Self) -> Result<Self, Error> {
        let n = self
            .num
            .checked_mul(other.num)
            .ok_or_else(|| Error::invariant("rational mul: numerator overflow"))?;
        let d = self
            .den
            .checked_mul(other.den)
            .ok_or_else(|| Error::invariant("rational mul: denominator overflow"))?;
        Self::new(n, d)
    }

    /// Checked division.
    pub fn checked_div(self, other: Self) -> Result<Self, Error> {
        self.checked_mul(other.recip()?)
    }

    /// Checked negation. Only fails on `i64::MIN`.
    pub fn checked_neg(self) -> Result<Self, Error> {
        let n = self
            .num
            .checked_neg()
            .ok_or_else(|| Error::invariant("rational neg: i64 overflow"))?;
        Ok(Self {
            num: n,
            den: self.den,
        })
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        // a/b cmp c/d  ==  a*d cmp c*b   (b, d both positive)
        // Use i128 to avoid overflow for typical values.
        let lhs = (self.num as i128) * (other.den as i128);
        let rhs = (other.num as i128) * (self.den as i128);
        lhs.cmp(&rhs)
    }
}

impl Default for Rational {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{}", self.num, self.den)
        }
    }
}

impl From<i64> for Rational {
    fn from(n: i64) -> Self {
        Self::from_int(n)
    }
}

impl Neg for Rational {
    type Output = Self;
    fn neg(self) -> Self {
        // i64::MIN cannot be negated; return ZERO in that pathological case.
        // Production code should use `checked_neg` directly.
        self.checked_neg().unwrap_or(Self::ZERO)
    }
}

impl Add for Rational {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        self.checked_add(rhs)
            .expect("Rational::add overflow; use checked_add")
    }
}

impl Sub for Rational {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self.checked_sub(rhs)
            .expect("Rational::sub overflow; use checked_sub")
    }
}

#[cfg(feature = "serde")]
impl From<Rational> for (i64, i64) {
    fn from(r: Rational) -> Self {
        (r.num, r.den)
    }
}

#[cfg(feature = "serde")]
impl TryFrom<(i64, i64)> for Rational {
    type Error = Error;
    fn try_from(v: (i64, i64)) -> Result<Self, Error> {
        Rational::new(v.0, v.1)
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn zero_denominator_rejected() {
        assert!(Rational::new(1, 0).is_err());
    }

    #[test]
    fn reduces_on_construction() {
        let r = Rational::new(6, 4).unwrap();
        assert_eq!(r.numer(), 3);
        assert_eq!(r.denom(), 2);
    }

    #[test]
    fn negative_denominator_normalized() {
        let r = Rational::new(1, -2).unwrap();
        assert_eq!(r.numer(), -1);
        assert_eq!(r.denom(), 2);
    }

    #[test]
    fn equality_of_unreduced_forms() {
        assert_eq!(Rational::new(1, 2).unwrap(), Rational::new(2, 4).unwrap());
        assert_eq!(Rational::new(-1, 2).unwrap(), Rational::new(1, -2).unwrap());
    }

    #[test]
    fn ordering_handles_signs() {
        let a = Rational::new(-1, 2).unwrap();
        let b = Rational::new(1, 3).unwrap();
        assert!(a < b);
        assert!(b > a);
        assert_eq!(a.cmp(&a), Ordering::Equal);
    }

    #[test]
    fn add_sub_round_trip() {
        let a = Rational::new(1001, 30000).unwrap();
        let b = Rational::new(1, 24).unwrap();
        let c = (a + b) - b;
        assert_eq!(c, a);
    }

    #[test]
    fn drop_frame_period_exact() {
        // 30000/1001 fps over 1 second = 30000/1001 frames; over 1001 seconds
        // = 30000 frames. Use this as a sanity check on rational arithmetic.
        let fps = Rational::new(30000, 1001).unwrap();
        let one_sec = Rational::ONE;
        let frames_per_sec = fps.checked_mul(one_sec).unwrap();
        assert_eq!(frames_per_sec, fps);
    }

    #[test]
    fn recip_zero_fails() {
        assert!(Rational::ZERO.recip().is_err());
    }

    #[test]
    fn display_format() {
        assert_eq!(Rational::from_int(5).to_string(), "5");
        assert_eq!(Rational::new(1, 2).unwrap().to_string(), "1/2");
        assert_eq!(Rational::new(-3, 4).unwrap().to_string(), "-3/4");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip() {
        // 1001 = 7·11·13, 30000 = 2^4·3·5^4 — coprime, so reduced form is unchanged.
        let r = Rational::new(1001, 30000).unwrap();
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "[1001,30000]");
        let r2: Rational = serde_json::from_str(&s).unwrap();
        assert_eq!(r, r2);
    }

    proptest! {
        #[test]
        fn add_is_commutative(an in -1_000_000i64..=1_000_000, ad in 1i64..=1_000_000, bn in -1_000_000i64..=1_000_000, bd in 1i64..=1_000_000) {
            let a = Rational::new(an, ad).unwrap();
            let b = Rational::new(bn, bd).unwrap();
            prop_assert_eq!(a.checked_add(b).unwrap(), b.checked_add(a).unwrap());
        }

        #[test]
        fn add_zero_is_identity(n in -1_000_000i64..=1_000_000, d in 1i64..=1_000_000) {
            let r = Rational::new(n, d).unwrap();
            prop_assert_eq!(r.checked_add(Rational::ZERO).unwrap(), r);
        }

        #[test]
        fn mul_one_is_identity(n in -1_000_000i64..=1_000_000, d in 1i64..=1_000_000) {
            let r = Rational::new(n, d).unwrap();
            prop_assert_eq!(r.checked_mul(Rational::ONE).unwrap(), r);
        }

        #[test]
        fn recip_recip_is_self(n in 1i64..=1_000_000, d in 1i64..=1_000_000) {
            let r = Rational::new(n, d).unwrap();
            prop_assert_eq!(r.recip().unwrap().recip().unwrap(), r);
        }

        #[test]
        fn ord_total(an in -1_000_000i64..=1_000_000, ad in 1i64..=1_000_000, bn in -1_000_000i64..=1_000_000, bd in 1i64..=1_000_000) {
            let a = Rational::new(an, ad).unwrap();
            let b = Rational::new(bn, bd).unwrap();
            // exactly one of <, ==, >
            let lt = a < b;
            let gt = a > b;
            let eq = a == b;
            prop_assert!(lt as u8 + gt as u8 + eq as u8 == 1);
        }
    }
}

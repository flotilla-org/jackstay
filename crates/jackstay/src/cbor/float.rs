//! Float forms: numeric reduction and the shortest exact width.

/// 2^64, the first float above the CBOR unsigned range.
const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// The integer an integral float stands for, if it must be encoded as one:
/// finite, without a fractional part, not `-0.0`, within `-2^64 ..= 2^64 - 1`.
pub(super) fn integral(f: f64) -> Option<i128> {
    if !f.is_finite() || f.fract() != 0.0 || (f == 0.0 && f.is_sign_negative()) {
        return None;
    }
    if !(-TWO_POW_64..TWO_POW_64).contains(&f) {
        return None;
    }
    // Exact: |f| <= 2^64 and f is integral.
    Some(f as i128)
}

/// Whether binary32 holds `f` exactly (infinities included; NaN excluded).
pub(super) fn fits_f32(f: f64) -> bool {
    !f.is_nan() && f64::from(f as f32).to_bits() == f.to_bits()
}

/// The binary16 bits that hold `f` exactly, if any (NaN excluded).
pub(super) fn to_f16(f: f64) -> Option<u16> {
    if !fits_f32(f) {
        return None;
    }
    let bits = (f as f32).to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;
    if exponent == 0xff {
        // Infinity: NaN was excluded above.
        return Some(sign | 0x7c00);
    }
    if exponent == 0 && mantissa == 0 {
        return Some(sign);
    }
    if exponent == 0 {
        // binary32 subnormals are far below the binary16 range.
        return None;
    }
    let unbiased = exponent - 127;
    match unbiased {
        -14..=15 if mantissa & 0x1fff == 0 => Some(sign | (((unbiased + 15) as u16) << 10) | (mantissa >> 13) as u16),
        -24..=-15 => {
            // A binary16 subnormal is k * 2^-24 with k < 2^10.
            let significand = mantissa | 0x80_0000;
            let shift = (-(unbiased + 1)) as u32;
            (significand & ((1 << shift) - 1) == 0).then_some(sign | (significand >> shift) as u16)
        }
        _ => None,
    }
}

/// The value of binary16 `bits`.
pub(super) fn from_f16(bits: u16) -> f64 {
    let exponent = i32::from((bits >> 10) & 0x1f);
    let mantissa = f64::from(bits & 0x3ff);
    let magnitude = match exponent {
        0 => mantissa * 2f64.powi(-24),
        0x1f if mantissa == 0.0 => f64::INFINITY,
        0x1f => f64::NAN,
        _ => (1024.0 + mantissa) * 2f64.powi(exponent - 25),
    };
    if bits & 0x8000 != 0 { -magnitude } else { magnitude }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_finite_f16_round_trips() {
        for bits in 0..=u16::MAX {
            let f = from_f16(bits);
            if f.is_nan() {
                assert_eq!(to_f16(f), None);
            } else {
                assert_eq!(to_f16(f), Some(bits), "{bits:#06x}");
            }
        }
    }

    #[test]
    fn widths() {
        assert_eq!(to_f16(65504.0), Some(0x7bff));
        assert_eq!(to_f16(65520.0), None);
        assert_eq!(to_f16(2f64.powi(-25)), None);
        assert!(fits_f32(2f64.powi(-149)));
        assert!(!fits_f32(0.1));
        assert!(fits_f32(f64::NEG_INFINITY));
    }

    #[test]
    fn reduction() {
        assert_eq!(integral(1.0), Some(1));
        assert_eq!(integral(0.0), Some(0));
        assert_eq!(integral(-0.0), None);
        assert_eq!(integral(-TWO_POW_64), Some(-(1 << 64)));
        assert_eq!(integral(TWO_POW_64), None);
        assert_eq!(integral(1e300), None);
        assert_eq!(integral(0.5), None);
        assert_eq!(integral(f64::INFINITY), None);
    }
}

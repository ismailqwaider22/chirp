//! Galois field GF(2^8) arithmetic for RLNC erasure coding.
//!
//! Irreducible polynomial: 0x11D (x^8 + x^4 + x^3 + x^2 + 1, AES standard).
//! Multiplication via log/antilog tables (256 entries each, compile-time generated).
//!
//! Fully `no_std + alloc` compatible.

/// Irreducible polynomial for GF(2^8): x^8 + x^4 + x^3 + x^2 + 1 = 0x11D.
const POLY: u16 = 0x11D;

/// Build the EXP (antilog) table at compile time.
/// Generator (primitive element) = 0x02; multiplication by 2 is a left shift.
/// exp_table[i] = GEN^i mod POLY for i in 0..255, with exp_table[255] = 0 as sentinel.
const fn build_exp_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut val: u16 = 1;
    let mut i = 0usize;
    while i < 255 {
        table[i] = val as u8;
        val <<= 1;
        if val & 0x100 != 0 {
            val ^= POLY;
        }
        i += 1;
    }
    // sentinel: exp_table[255] = 0 (never used for valid log lookups)
    table[255] = 0;
    table
}

/// Build the LOG table at compile time.
/// log_table[exp_table[i]] = i for i in 0..255. log_table[0] is undefined (set to 0).
const fn build_log_table() -> [u8; 256] {
    let exp = build_exp_table();
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 255 {
        table[exp[i] as usize] = i as u8;
        i += 1;
    }
    table
}

static EXP_TABLE: [u8; 256] = build_exp_table();
static LOG_TABLE: [u8; 256] = build_log_table();

/// GF(2^8) addition: XOR.
#[inline(always)]
pub fn gf_add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// GF(2^8) multiplication via log/antilog tables.
#[inline(always)]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let log_sum = LOG_TABLE[a as usize] as u16 + LOG_TABLE[b as usize] as u16;
    EXP_TABLE[(log_sum % 255) as usize]
}

/// GF(2^8) multiplicative inverse: a^{-1} = a^{253} via log table.
#[inline(always)]
pub fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0, "gf_inv(0) is undefined");
    if a == 0 {
        return 0;
    }
    let neg_log = (255 - LOG_TABLE[a as usize] as u16) % 255;
    EXP_TABLE[neg_log as usize]
}

/// GF(2^8) division: a / b.
#[inline(always)]
pub fn gf_div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0, "gf_div by zero");
    if a == 0 || b == 0 {
        return 0;
    }
    let log_diff = LOG_TABLE[a as usize] as i16 - LOG_TABLE[b as usize] as i16;
    let idx = ((log_diff % 255 + 255) % 255) as usize;
    EXP_TABLE[idx]
}

/// Multiply every byte of `vec` by `scalar` in GF(2^8), in place.
#[inline]
pub fn gf_vec_mul_scalar(vec: &mut [u8], scalar: u8) {
    if scalar == 0 {
        for b in vec.iter_mut() {
            *b = 0;
        }
        return;
    }
    if scalar == 1 {
        return;
    }
    let log_s = LOG_TABLE[scalar as usize] as u16;
    for b in vec.iter_mut() {
        if *b != 0 {
            let log_sum = LOG_TABLE[*b as usize] as u16 + log_s;
            *b = EXP_TABLE[(log_sum % 255) as usize];
        }
    }
}

/// dst[i] ^= scalar * src[i] over GF(2^8) — the critical RLNC inner loop.
///
/// On x86_64 with AVX2 (std feature + runtime CPUID), uses a SIMD path.
/// Otherwise falls back to the scalar log/antilog implementation.
#[inline]
pub fn gf_vec_add_mul(dst: &mut [u8], src: &[u8], scalar: u8) {
    let len = dst.len().min(src.len());
    if scalar == 0 {
        return;
    }
    if scalar == 1 {
        for i in 0..len {
            dst[i] ^= src[i];
        }
        return;
    }

    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    {
        if is_x86_feature_detected!("avx2") {
            // Safety: we checked AVX2 support above.
            unsafe {
                gf_vec_add_mul_avx2(&mut dst[..len], &src[..len], scalar);
            }
            return;
        }
    }

    gf_vec_add_mul_scalar(&mut dst[..len], &src[..len], scalar);
}

/// Scalar fallback for gf_vec_add_mul.
#[inline]
fn gf_vec_add_mul_scalar(dst: &mut [u8], src: &[u8], scalar: u8) {
    let log_s = LOG_TABLE[scalar as usize] as u16;
    for i in 0..dst.len() {
        if src[i] != 0 {
            let log_sum = LOG_TABLE[src[i] as usize] as u16 + log_s;
            dst[i] ^= EXP_TABLE[(log_sum % 255) as usize];
        }
    }
}

/// AVX2 SIMD path for gf_vec_add_mul.
///
/// Strategy: use the split-table approach. For each byte src[i], compute
/// gf_mul(src[i], scalar) via a pair of 16-entry lookup tables (low/high nibble)
/// using `vpshufb`, then XOR into dst.
#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn gf_vec_add_mul_avx2(dst: &mut [u8], src: &[u8], scalar: u8) {
    use core::arch::x86_64::*;

    let len = dst.len();
    if len == 0 {
        return;
    }

    // Build the two 16-entry lookup tables for split multiplication:
    // lo_lut[i] = gf_mul(i, scalar) for i in 0..16
    // hi_lut[i] = gf_mul(i << 4, scalar) for i in 0..16
    let mut lo_lut = [0u8; 16];
    let mut hi_lut = [0u8; 16];
    for i in 0..16u8 {
        lo_lut[i as usize] = gf_mul(i, scalar);
        hi_lut[i as usize] = gf_mul(i << 4, scalar);
    }

    // Broadcast each 16-byte LUT into both 128-bit lanes of a 256-bit register.
    let lo_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(lo_lut.as_ptr() as *const _));
    let hi_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(hi_lut.as_ptr() as *const _));
    let mask_0f = _mm256_set1_epi8(0x0F);

    let mut i = 0usize;
    // Process 32 bytes at a time.
    while i + 32 <= len {
        let s = _mm256_loadu_si256(src.as_ptr().add(i) as *const _);
        let d = _mm256_loadu_si256(dst.as_ptr().add(i) as *const _);

        let lo_nibble = _mm256_and_si256(s, mask_0f);
        let hi_nibble = _mm256_and_si256(_mm256_srli_epi16(s, 4), mask_0f);

        let prod_lo = _mm256_shuffle_epi8(lo_vec, lo_nibble);
        let prod_hi = _mm256_shuffle_epi8(hi_vec, hi_nibble);
        let product = _mm256_xor_si256(prod_lo, prod_hi);

        let result = _mm256_xor_si256(d, product);
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut _, result);

        i += 32;
    }

    // Scalar tail for remaining bytes.
    if i < len {
        gf_vec_add_mul_scalar(&mut dst[i..], &src[i..], scalar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_table_sanity() {
        assert_eq!(EXP_TABLE[0], 1); // g^0 = 1
        assert_eq!(EXP_TABLE[1], 2); // g^1 = 2 (generator)
    }

    #[test]
    fn log_exp_inverse() {
        for i in 1u16..=255 {
            let a = i as u8;
            assert_eq!(EXP_TABLE[LOG_TABLE[a as usize] as usize], a);
        }
    }

    #[test]
    fn mul_identity() {
        for a in 0u16..=255 {
            assert_eq!(gf_mul(a as u8, 1), a as u8);
            assert_eq!(gf_mul(1, a as u8), a as u8);
            assert_eq!(gf_mul(a as u8, 0), 0);
            assert_eq!(gf_mul(0, a as u8), 0);
        }
    }

    #[test]
    fn mul_inverse() {
        for a in 1u16..=255 {
            let a = a as u8;
            let inv = gf_inv(a);
            assert_eq!(gf_mul(a, inv), 1, "a={a}, inv={inv}");
        }
    }

    #[test]
    fn div_roundtrip() {
        for a in 1u16..=255 {
            for b in 1u16..=255 {
                let a = a as u8;
                let b = b as u8;
                let c = gf_mul(a, b);
                assert_eq!(gf_div(c, b), a, "a={a}, b={b}, c={c}");
            }
        }
    }

    #[test]
    fn vec_add_mul_matches_scalar() {
        let src: Vec<u8> = (0..=255).collect();
        let scalar = 0x53;
        let mut dst_a = vec![0xABu8; 256];
        let mut dst_b = dst_a.clone();

        // Scalar path
        gf_vec_add_mul_scalar(&mut dst_a, &src, scalar);
        // Full path (may use SIMD)
        gf_vec_add_mul(&mut dst_b, &src, scalar);

        assert_eq!(dst_a, dst_b);
    }

    #[test]
    fn add_is_xor() {
        assert_eq!(gf_add(0xFF, 0xFF), 0);
        assert_eq!(gf_add(0xAB, 0x00), 0xAB);
        assert_eq!(gf_add(0x12, 0x34), 0x12 ^ 0x34);
    }
}

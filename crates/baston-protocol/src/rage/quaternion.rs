//! Smallest-three quaternion compression, as the orientation sync nodes use.
//!
//! Port of the engine's `compressed_quaternion<bits>` (`shared/state/kumquat.h`).
//! A unit quaternion has one redundant component, so the wire carries a 2-bit
//! index naming the largest-magnitude component and the other three quantised
//! over `[-1/√2, +1/√2]` — the range the three smallest components are confined
//! to once the largest is known.
//!
//! Both directions live here because the constants have to agree exactly: the
//! engine divides by the literal `1.414214`, not by `sqrt(2)`, and reproducing
//! that literal is what makes a round trip land on the same integers a real
//! client would send.

/// The engine's own approximation of √2.
///
/// Clippy is right that this is `f32::consts::SQRT_2` rounded — and wrong that
/// we should use the exact one. This constant sets the quantisation range, so
/// it has to be bit-identical to the value clients encode against; the precise
/// constant would shift every component by a fraction of a step and put our
/// integers next to theirs instead of on them.
#[allow(clippy::approx_constant)]
const SQRT_2: f32 = 1.414_214;
const MINIMUM: f32 = -1.0 / SQRT_2;
const MAXIMUM: f32 = 1.0 / SQRT_2;

/// Bits per component in every orientation node that carries a quaternion.
pub const BITS: u32 = 11;

/// A full turn in radians — the divisor the ped heading fields quantise over.
/// Spelled as the engine spells it, not as `std::f32::consts::TAU`, so the
/// quantisation lands on the same steps a client produces.
pub const TAU_RADIANS: f32 = 6.283_185_5;

/// A quaternion in the wire's compressed form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressedQuaternion {
    /// Index (0..=3) of the component with the largest magnitude, in `xyzw`
    /// order. It is not transmitted: the receiver rebuilds it from the others.
    pub largest: u32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

/// Compress a unit quaternion to `bits` per component.
#[must_use]
pub fn compress(quaternion: [f32; 4], bits: u32) -> CompressedQuaternion {
    let scale = ((1_u32 << bits) - 1) as f32;
    let [x, y, z, w] = quaternion;

    // Index of the largest-magnitude component.
    let mut largest = 0_u32;
    let mut largest_value = x.abs();
    for (index, value) in [y.abs(), z.abs(), w.abs()].into_iter().enumerate() {
        if value > largest_value {
            largest = index as u32 + 1;
            largest_value = value;
        }
    }

    // The three others, sign-flipped when the largest is negative so the
    // receiver can always rebuild it as a positive square root.
    let (largest_component, others) = match largest {
        0 => (x, [y, z, w]),
        1 => (y, [x, z, w]),
        2 => (z, [x, y, w]),
        _ => (w, [x, y, z]),
    };
    let sign = if largest_component >= 0.0 { 1.0 } else { -1.0 };

    let quantise = |value: f32| {
        let normal = (value * sign - MINIMUM) / (MAXIMUM - MINIMUM);
        (normal * scale + 0.5).floor() as u32
    };

    CompressedQuaternion {
        largest,
        a: quantise(others[0]),
        b: quantise(others[1]),
        c: quantise(others[2]),
    }
}

/// Rebuild a unit quaternion (`xyzw`) from its compressed form.
#[must_use]
pub fn decompress(compressed: CompressedQuaternion, bits: u32) -> [f32; 4] {
    let scale = ((1_u32 << bits) - 1) as f32;
    let dequantise = |value: u32| (value as f32 / scale) * (MAXIMUM - MINIMUM) + MINIMUM;
    let (a, b, c) = (
        dequantise(compressed.a),
        dequantise(compressed.b),
        dequantise(compressed.c),
    );
    // The dropped component is whatever makes the quaternion a unit one. A
    // corrupt payload can push the sum past 1; clamp so the square root stays
    // real instead of yielding NaN.
    let largest = (1.0 - a * a - b * b - c * c).max(0.0).sqrt();

    match compressed.largest {
        0 => [largest, a, b, c],
        1 => [a, largest, b, c],
        2 => [a, b, largest, c],
        _ => [a, b, c, largest],
    }
}

/// The quaternion for a GTA heading, in degrees.
///
/// Mirrors the engine's `glm::quat(glm::vec3(0, 0, heading * DEG_TO_RAD))`:
/// with pitch and roll zero, that reduces to a rotation about Z.
#[must_use]
pub fn from_heading_degrees(heading: f32) -> [f32; 4] {
    let half = heading.to_radians() * 0.5;
    [0.0, 0.0, half.sin(), half.cos()]
}

/// The heading, in degrees, a quaternion represents about the Z axis.
#[must_use]
pub fn to_heading_degrees(quaternion: [f32; 4]) -> f32 {
    let [x, y, z, w] = quaternion;
    let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    let degrees = yaw.to_degrees();
    // GTA headings are 0..360.
    if degrees < 0.0 {
        degrees + 360.0
    } else {
        degrees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BITS: u32 = 11;

    /// Shortest angular distance between two headings, so 359.99° and 0.01°
    /// count as adjacent rather than a full turn apart.
    fn angular_error(a: f32, b: f32) -> f32 {
        (((a - b) + 540.0).rem_euclid(360.0) - 180.0).abs()
    }

    #[test]
    fn a_heading_survives_the_round_trip() {
        for heading in [0.0_f32, 45.0, 90.0, 179.0, 180.0, 270.0, 359.0] {
            let compressed = compress(from_heading_degrees(heading), BITS);
            let restored = to_heading_degrees(decompress(compressed, BITS));
            assert!(
                angular_error(restored, heading) < 0.5,
                "heading {heading} came back as {restored}"
            );
        }
    }

    /// 11 bits over the smallest-three range is fine enough that no heading is
    /// off by more than a fraction of a degree.
    #[test]
    fn quantisation_error_stays_below_a_tenth_of_a_degree() {
        let worst = (0..3600)
            .map(|tenth| {
                let heading = tenth as f32 / 10.0;
                let restored = to_heading_degrees(decompress(
                    compress(from_heading_degrees(heading), BITS),
                    BITS,
                ));
                angular_error(restored, heading)
            })
            .fold(0.0_f32, f32::max);
        assert!(worst < 0.1, "worst-case error {worst}°");
    }

    /// The compressed form must actually fit the field widths it is written
    /// into, or the bit writer would silently truncate it.
    #[test]
    fn components_fit_their_field_widths() {
        let max = (1_u32 << BITS) - 1;
        for heading in 0..360 {
            let c = compress(from_heading_degrees(heading as f32), BITS);
            assert!(c.largest < 4, "largest index fits 2 bits");
            for component in [c.a, c.b, c.c] {
                assert!(component <= max, "component {component} exceeds {max}");
            }
        }
    }

    #[test]
    fn the_largest_component_is_the_one_reconstructed() {
        // A pure Z rotation of 90°: z and w are equal in magnitude, everything
        // else is zero, so the dropped component must be z or w.
        let compressed = compress(from_heading_degrees(90.0), BITS);
        assert!(matches!(compressed.largest, 2 | 3));
    }

    /// Decompression must never produce NaN, whatever the payload says.
    #[test]
    fn a_corrupt_payload_decompresses_to_a_real_quaternion() {
        let max = (1_u32 << BITS) - 1;
        for largest in 0..4 {
            let quaternion = decompress(
                CompressedQuaternion {
                    largest,
                    a: max,
                    b: max,
                    c: max,
                },
                BITS,
            );
            assert!(
                quaternion.iter().all(|component| component.is_finite()),
                "{quaternion:?}"
            );
        }
    }
}

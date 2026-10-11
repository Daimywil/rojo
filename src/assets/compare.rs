//! Compares asset files while ignoring floating point noise.
//!
//! Studio doesn't always load and save a value exactly. Loading a model and
//! saving it again can change the last bit of a float, like one entry of a
//! bone's rotation, and change it back the next time. Treating that as an edit
//! would rewrite files and ask the user about changes nobody made.
//!
//! Only floats may differ, and only by rounding. Every other byte of both
//! files has to be the same, so anything this doesn't understand, like a type
//! of value that's new to it, counts as a real difference.

use std::collections::HashMap;

use super::rbxm;

/// Floats at most this many representable values apart count as the same,
/// which covers rounding in their last bits whatever their magnitude.
const MAX_ULPS: i64 = 4;

/// Floats at most this far apart count as the same too, since rounding noise
/// around zero spans far more representable values than anywhere else.
const NEAR_ZERO_F32: f64 = 1e-6;
const NEAR_ZERO_F64: f64 = 1e-12;

// The value types of the binary format that hold floats.
const FLOAT32: u8 = 0x04;
const FLOAT64: u8 = 0x05;
const UDIM: u8 = 0x06;
const UDIM2: u8 = 0x07;
const RAY: u8 = 0x08;
const COLOR3: u8 = 0x0C;
const VECTOR2: u8 = 0x0D;
const VECTOR3: u8 = 0x0E;
const CFRAME: u8 = 0x10;
const NUMBER_SEQUENCE: u8 = 0x15;
const COLOR_SEQUENCE: u8 = 0x16;
const NUMBER_RANGE: u8 = 0x17;
const RECT: u8 = 0x18;
const PHYSICAL_PROPERTIES: u8 = 0x19;

/// Whether two binary models are the same apart from floats that only differ
/// by rounding. Models that can't be read are never the same.
pub fn same_except_float_noise(a: &[u8], b: &[u8]) -> bool {
    let (Ok(a), Ok(b)) = (rbxm::chunks(a), rbxm::chunks(b)) else {
        return false;
    };

    if a.header != b.header || a.trailing != b.trailing || a.chunks.len() != b.chunks.len() {
        return false;
    }

    // A property chunk holds one value for every instance of its class, which
    // is the only way to tell where one value ends and the next one starts.
    // INST chunks have to be identical, so either model's counts will do.
    let counts: HashMap<u32, usize> = a
        .chunks
        .iter()
        .filter(|chunk| &chunk.name == b"INST")
        .filter_map(|chunk| instance_count(&chunk.data))
        .collect();

    a.chunks.iter().zip(&b.chunks).all(|(a, b)| {
        a.name == b.name
            && a.reserved == b.reserved
            && (a.data == b.data
                || (&a.name == b"PROP" && same_property_values(&a.data, &b.data, &counts)))
    })
}

/// Reads the class ID and the number of instances of an INST chunk.
fn instance_count(data: &[u8]) -> Option<(u32, usize)> {
    let class = read_u32(data, 0)?;
    let name_len = read_u32(data, 4)? as usize;
    // The class name is followed by a byte that tells whether it's a service.
    let count = read_u32(data, 8 + name_len + 1)? as usize;
    Some((class, count))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Whether two PROP chunks are for the same property and their values are the
/// same apart from float noise.
fn same_property_values(a: &[u8], b: &[u8], counts: &HashMap<u32, usize>) -> bool {
    let Some(class) = read_u32(a, 0) else {
        return false;
    };
    let Some(name_len) = read_u32(a, 4) else {
        return false;
    };

    // The class, the property's name, and the type of its values.
    let header_len = 8 + name_len as usize + 1;
    let (Some(a_header), Some(b_header)) = (a.get(..header_len), b.get(..header_len)) else {
        return false;
    };
    let Some(&count) = counts.get(&class) else {
        return false;
    };

    if a_header != b_header {
        return false;
    }

    let value_type = a_header[header_len - 1];
    let mut values = Values {
        a: &a[header_len..],
        b: &b[header_len..],
    };

    same_values(&mut values, value_type, count) == Some(true)
        && values.a.is_empty()
        && values.b.is_empty()
}

/// Compares `count` values of the given type, or returns `None` if either side
/// isn't laid out the way that type is.
fn same_values(values: &mut Values, value_type: u8, count: usize) -> Option<bool> {
    let same = match value_type {
        FLOAT32 => values.interleaved_f32s(count)?,
        FLOAT64 => values.f64s(count)?,
        // A scale for each value, then an offset, which is an integer.
        UDIM => values.interleaved_f32s(count)? && values.exact(count.checked_mul(4)?)?,
        UDIM2 => {
            values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
                && values.exact(count.checked_mul(8)?)?
        }
        RAY => values.f32s(count.checked_mul(6)?)?,
        COLOR3 | VECTOR3 => {
            values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
        }
        VECTOR2 => values.interleaved_f32s(count)? && values.interleaved_f32s(count)?,
        RECT => {
            values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
        }
        NUMBER_RANGE => values.f32s(count.checked_mul(2)?)?,
        CFRAME => {
            // Every rotation is either the ID of an axis-aligned rotation, or
            // 0 followed by the whole matrix. The positions come after them.
            for _ in 0..count {
                let (a, b) = values.byte()?;
                if a != b || (a == 0 && !values.f32s(9)?) {
                    return Some(false);
                }
            }
            values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
                && values.interleaved_f32s(count)?
        }
        NUMBER_SEQUENCE | COLOR_SEQUENCE => {
            // A time, value, and envelope per keypoint, or a time, three color
            // components, and an unused envelope.
            let floats_per_keypoint = if value_type == NUMBER_SEQUENCE { 3 } else { 5 };
            for _ in 0..count {
                let (a, b) = values.u32()?;
                if a != b || !values.f32s((a as usize).checked_mul(floats_per_keypoint)?)? {
                    return Some(false);
                }
            }
            true
        }
        PHYSICAL_PROPERTIES => {
            // Default properties have no floats, and custom ones have five, or
            // six with acoustic absorption.
            for _ in 0..count {
                let (a, b) = values.byte()?;
                let floats = match a {
                    0b00 | 0b10 => 0,
                    0b01 => 5,
                    0b11 => 6,
                    _ => return None,
                };
                if a != b || !values.f32s(floats)? {
                    return Some(false);
                }
            }
            true
        }
        // Values without floats only count as the same if they're identical,
        // which these weren't.
        _ => false,
    };

    Some(same)
}

/// The values of two PROP chunks, read side by side.
struct Values<'a> {
    a: &'a [u8],
    b: &'a [u8],
}

impl<'a> Values<'a> {
    fn take(&mut self, len: usize) -> Option<(&'a [u8], &'a [u8])> {
        if self.a.len() < len || self.b.len() < len {
            return None;
        }

        let (a, a_rest) = self.a.split_at(len);
        let (b, b_rest) = self.b.split_at(len);
        self.a = a_rest;
        self.b = b_rest;
        Some((a, b))
    }

    /// Bytes that have to be identical, like integers.
    fn exact(&mut self, len: usize) -> Option<bool> {
        let (a, b) = self.take(len)?;
        Some(a == b)
    }

    fn byte(&mut self) -> Option<(u8, u8)> {
        let (a, b) = self.take(1)?;
        Some((a[0], b[0]))
    }

    fn u32(&mut self) -> Option<(u32, u32)> {
        let (a, b) = self.take(4)?;
        Some((
            u32::from_le_bytes(a.try_into().unwrap()),
            u32::from_le_bytes(b.try_into().unwrap()),
        ))
    }

    /// `count` little-endian f32s, one after the other.
    fn f32s(&mut self, count: usize) -> Option<bool> {
        let (a, b) = self.take(count.checked_mul(4)?)?;
        Some(a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(a, b)| {
            close_f32(
                f32::from_le_bytes(a.try_into().unwrap()),
                f32::from_le_bytes(b.try_into().unwrap()),
            )
        }))
    }

    /// `count` little-endian f64s, one after the other.
    fn f64s(&mut self, count: usize) -> Option<bool> {
        let (a, b) = self.take(count.checked_mul(8)?)?;
        Some(a.chunks_exact(8).zip(b.chunks_exact(8)).all(|(a, b)| {
            close_f64(
                f64::from_le_bytes(a.try_into().unwrap()),
                f64::from_le_bytes(b.try_into().unwrap()),
            )
        }))
    }

    /// `count` f32s the way most arrays of floats are stored: big-endian, with
    /// the sign moved to the lowest bit, and the bytes interleaved so that the
    /// first bytes of every value come first.
    fn interleaved_f32s(&mut self, count: usize) -> Option<bool> {
        let (a, b) = self.take(count.checked_mul(4)?)?;
        let read = |bytes: &[u8], index: usize| {
            let be = [
                bytes[index],
                bytes[index + count],
                bytes[index + count * 2],
                bytes[index + count * 3],
            ];
            f32::from_bits(u32::from_be_bytes(be).rotate_right(1))
        };

        Some((0..count).all(|index| close_f32(read(a, index), read(b, index))))
    }
}

fn close_f32(a: f32, b: f32) -> bool {
    // Maps floats to integers such that neighboring floats map to neighboring
    // integers, so their difference counts the floats between them.
    fn ordered(value: f32) -> i64 {
        let bits = i64::from(value.to_bits() as i32);
        if bits < 0 {
            i64::from(i32::MIN) - bits
        } else {
            bits
        }
    }

    a == b
        || (a.is_nan() && b.is_nan())
        || (a.is_finite()
            && b.is_finite()
            && ((ordered(a) - ordered(b)).abs() <= MAX_ULPS
                || (f64::from(a) - f64::from(b)).abs() <= NEAR_ZERO_F32))
}

fn close_f64(a: f64, b: f64) -> bool {
    fn ordered(value: f64) -> i128 {
        let bits = i128::from(value.to_bits() as i64);
        if bits < 0 {
            i128::from(i64::MIN) - bits
        } else {
            bits
        }
    }

    a == b
        || (a.is_nan() && b.is_nan())
        || (a.is_finite()
            && b.is_finite()
            && ((ordered(a) - ordered(b)).abs() <= i128::from(MAX_ULPS)
                || (a - b).abs() <= NEAR_ZERO_F64))
}

#[cfg(test)]
mod test {
    use super::*;

    use rbx_dom_weak::{
        types::{
            CFrame, Color3, CustomPhysicalProperties, Matrix3, NumberSequence,
            NumberSequenceKeypoint, PhysicalProperties, UDim, UDim2, Variant, Vector3,
        },
        InstanceBuilder, WeakDom,
    };

    /// The next float up, the smallest possible change.
    fn bump(value: f32) -> f32 {
        f32::from_bits(value.to_bits() + 1)
    }

    fn model(properties: Vec<(&str, Variant)>) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Model").with_name("Arm"),
        );
        dom.insert(
            model,
            InstanceBuilder::new("Part")
                .with_name("Palm")
                .with_properties(properties),
        );

        let mut output = Vec::new();
        rbx_binary::to_writer(&mut output, &dom, &[model]).unwrap();
        output
    }

    fn rotated(xx: f32) -> CFrame {
        CFrame::new(
            Vector3::new(0.0, 2.145, 0.0),
            Matrix3::new(
                Vector3::new(xx, -0.151, -0.435),
                Vector3::new(-0.361, 0.356, -0.861),
                Vector3::new(0.285, 0.921, 0.261),
            ),
        )
    }

    #[test]
    fn identical_models_are_the_same() {
        let a = model(vec![("Transparency", 0.25f32.into())]);
        assert!(same_except_float_noise(&a, &a.clone()));
    }

    #[test]
    fn rounding_in_rotations_is_ignored() {
        let a = model(vec![("CFrame", rotated(0.887).into())]);
        let b = model(vec![("CFrame", rotated(bump(bump(0.887))).into())]);
        assert_ne!(a, b);
        assert!(same_except_float_noise(&a, &b));
    }

    #[test]
    fn real_changes_are_not_ignored() {
        let a = model(vec![("CFrame", rotated(0.887).into())]);
        let b = model(vec![("CFrame", rotated(0.888).into())]);
        assert!(!same_except_float_noise(&a, &b));

        // Even far from the origin, where floats are coarse, a move of a
        // hundredth of a stud is a real change.
        let far = |x: f32| CFrame::new(Vector3::new(x, 0.0, 0.0), Matrix3::identity()).into();
        let a = model(vec![("CFrame", far(10_000.0))]);
        let b = model(vec![("CFrame", far(10_000.01))]);
        assert!(!same_except_float_noise(&a, &b));
    }

    /// The data of a PROP chunk for the CFrame of a single instance of class
    /// 0, with the given rotation.
    fn cframe_chunk(rotation: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&6u32.to_le_bytes());
        data.extend_from_slice(b"CFrame");
        data.push(CFRAME);
        data.extend_from_slice(rotation);
        // Three interleaved arrays of one float, which aren't interleaved at
        // all with just one value.
        for value in [1.0f32, 2.0, 3.0] {
            data.extend_from_slice(&value.to_bits().rotate_left(1).to_be_bytes());
        }
        data
    }

    #[test]
    fn axis_aligned_rotations_have_to_stay_axis_aligned() {
        // rbx_binary writes rotations this close to an axis-aligned one as
        // that rotation's ID, so these chunks are put together by hand.
        let counts = HashMap::from([(0, 1)]);
        let aligned = cframe_chunk(&[0x02]);
        let mut matrix = vec![0];
        for value in [bump(1.0), 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            matrix.extend_from_slice(&value.to_le_bytes());
        }
        let almost = cframe_chunk(&matrix);

        assert!(same_property_values(&aligned, &aligned, &counts));
        assert!(same_property_values(&almost, &almost, &counts));
        assert!(!same_property_values(&aligned, &almost, &counts));
    }

    #[test]
    fn rounding_near_zero_is_ignored() {
        let a = model(vec![("Transparency", 0.0f32.into())]);
        let b = model(vec![("Transparency", (-3e-8f32).into())]);
        assert!(same_except_float_noise(&a, &b));
    }

    #[test]
    fn rounding_in_other_float_types_is_ignored() {
        let sequence = |value: f32| {
            NumberSequence {
                keypoints: vec![
                    NumberSequenceKeypoint::new(0.0, value, 0.0),
                    NumberSequenceKeypoint::new(1.0, 0.5, 0.0),
                ],
            }
            .into()
        };
        let physical = |density: f32| {
            PhysicalProperties::Custom(CustomPhysicalProperties::new(
                density, 0.3, 0.5, 1.0, 1.0, 1.0,
            ))
            .into()
        };
        let properties = |value: f32| {
            vec![
                ("Size", Vector3::new(value, 1.0, 2.0).into()),
                // Part.Color is stored as bytes, so this is a made-up property
                // to get a Color3 stored as floats.
                ("Color3Test", Color3::new(value, 0.5, 0.25).into()),
                ("CustomPhysicalProperties", physical(value)),
                ("NumberSequenceTest", sequence(value)),
                (
                    "UDim2Test",
                    UDim2::new(UDim::new(value, 10), UDim::new(0.5, 20)).into(),
                ),
            ]
        };

        let a = model(properties(0.7));
        let b = model(properties(bump(0.7)));
        assert_ne!(a, b);
        assert!(same_except_float_noise(&a, &b));
        assert!(!same_except_float_noise(&a, &model(properties(0.75))));
    }

    #[test]
    fn integers_have_to_match_exactly() {
        let udim2 = |offset: i32| UDim2::new(UDim::new(0.5, offset), UDim::new(0.5, 0)).into();
        let a = model(vec![("UDim2Test", udim2(10))]);
        let b = model(vec![("UDim2Test", udim2(11))]);
        assert!(!same_except_float_noise(&a, &b));
    }

    #[test]
    fn values_without_floats_have_to_match_exactly() {
        let a = model(vec![("Transparency", 0.5f32.into())]);
        let mut renamed = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = renamed.root_ref();
        let arm = renamed.insert(root, InstanceBuilder::new("Model").with_name("Arm"));
        renamed.insert(
            arm,
            InstanceBuilder::new("Part")
                .with_name("Wrist")
                .with_property("Transparency", 0.5f32),
        );
        let mut b = Vec::new();
        rbx_binary::to_writer(&mut b, &renamed, &[arm]).unwrap();

        assert!(!same_except_float_noise(&a, &b));
    }

    #[test]
    fn models_with_different_instances_are_different() {
        let a = model(vec![]);
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let arm = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Model").with_name("Arm"),
        );
        dom.insert(arm, InstanceBuilder::new("Part").with_name("Palm"));
        dom.insert(arm, InstanceBuilder::new("Part").with_name("Palm"));
        let mut b = Vec::new();
        rbx_binary::to_writer(&mut b, &dom, &[arm]).unwrap();

        assert!(!same_except_float_noise(&a, &b));
    }

    #[test]
    fn invalid_models_are_never_the_same() {
        let a = model(vec![]);
        assert!(!same_except_float_noise(&a, b"not a model"));
        assert!(!same_except_float_noise(b"not a model", b"not a model"));
    }
}

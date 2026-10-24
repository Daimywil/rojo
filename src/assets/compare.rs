//! Compares asset files while ignoring the differences Studio makes on its own.
//!
//! Studio doesn't save an asset the same way every time, even when nobody
//! changed it:
//!
//! - Loading a model and saving it again can change the last bit of a float,
//!   like one entry of a bone's rotation, and change it back the next time.
//! - Instances are stored in the order of their siblings, which is the order
//!   they were parented in. An instance that's parented somewhere else and
//!   back becomes the last of its siblings, and every instance after it in the
//!   file moves along with it, without anything about them changing.
//! - Every instance has a `UniqueId`, which only identifies it, and which
//!   Studio gives copies of an instance new values for.
//!
//! Treating any of that as an edit would rewrite files and ask the user about
//! changes nobody made. So models are compared as trees of instances rather
//! than as bytes: siblings may be in any order, floats may differ by rounding,
//! and `UniqueId`s may differ. Everything else has to be identical, including
//! integers, rotation IDs, and the instances that references point to.
//! Models with anything this doesn't know how to read, like a type of value
//! that's new to it, are never the same.

use std::{
    borrow::Cow,
    collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet},
    fmt,
    hash::{Hash, Hasher},
};

use super::rbxm;

/// Floats at most this many representable values apart count as the same,
/// which covers rounding in their last bits whatever their magnitude.
const MAX_ULPS: i64 = 4;

/// Floats at most this far apart count as the same too, since rounding noise
/// around zero spans far more representable values than anywhere else.
const NEAR_ZERO_F32: f64 = 1e-6;
const NEAR_ZERO_F64: f64 = 1e-12;

/// Instances nested deeper than this are never compared, which keeps the
/// recursion below from running out of stack. Real models don't come close.
const MAX_DEPTH: usize = 512;

/// How many pairs of instances matching may try for every instance a model
/// has. Siblings that look alike are matched by trying them against each
/// other, and this keeps that from taking forever. Models that need more tries
/// count as different.
const TRIES_PER_INSTANCE: usize = 16;

// The value types of the binary format.
const STRING: u8 = 0x01;
const BOOL: u8 = 0x02;
const INT32: u8 = 0x03;
const FLOAT32: u8 = 0x04;
const FLOAT64: u8 = 0x05;
const UDIM: u8 = 0x06;
const UDIM2: u8 = 0x07;
const RAY: u8 = 0x08;
const FACES: u8 = 0x09;
const AXES: u8 = 0x0A;
const BRICK_COLOR: u8 = 0x0B;
const COLOR3: u8 = 0x0C;
const VECTOR2: u8 = 0x0D;
const VECTOR3: u8 = 0x0E;
const CFRAME: u8 = 0x10;
const ENUM: u8 = 0x12;
const REF: u8 = 0x13;
const VECTOR3_INT16: u8 = 0x14;
const NUMBER_SEQUENCE: u8 = 0x15;
const COLOR_SEQUENCE: u8 = 0x16;
const NUMBER_RANGE: u8 = 0x17;
const RECT: u8 = 0x18;
const PHYSICAL_PROPERTIES: u8 = 0x19;
const COLOR3_UINT8: u8 = 0x1A;
const INT64: u8 = 0x1B;
const SHARED_STRING: u8 = 0x1C;
const OPTIONAL_CFRAME: u8 = 0x1E;
const UNIQUE_ID: u8 = 0x1F;
const FONT: u8 = 0x20;
const SECURITY_CAPABILITIES: u8 = 0x21;
const CONTENT: u8 = 0x22;

/// Why an asset file and Studio's version of it aren't the same.
#[derive(Debug)]
pub struct Difference(String);

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks whether an asset file and Studio's version of it hold the same
/// instances, apart from the differences Studio makes on its own (see the
/// module's documentation).
pub fn compare(file: &[u8], studio: &[u8]) -> Result<(), Difference> {
    let file = uncompressed(file).map_err(|err| Difference(format!("the file {err}")))?;
    let studio =
        uncompressed(studio).map_err(|err| Difference(format!("Studio's version {err}")))?;
    let file = Model::parse(&file).map_err(|err| Difference(format!("the file {err}")))?;
    let studio =
        Model::parse(&studio).map_err(|err| Difference(format!("Studio's version {err}")))?;

    if file.header != studio.header {
        return Err(Difference(
            "they have a different number of instances or classes".to_owned(),
        ));
    }
    if file.metadata != studio.metadata {
        return Err(Difference("their metadata differs".to_owned()));
    }
    if file.end != studio.end || file.trailing != studio.trailing {
        return Err(Difference("they end differently".to_owned()));
    }

    let mut matcher = Matcher::new(&file, &studio);
    if matcher.match_children(&file.roots, &studio.roots) && matcher.references_match() {
        return Ok(());
    }

    Err(Difference(if matcher.tries == 0 {
        "they have too many instances that look alike to compare".to_owned()
    } else {
        explain(&file, &studio)
    }))
}

/// The model with none of its chunks compressed, which is how asset files are
/// stored, but not how Studio uploads them.
fn uncompressed(contents: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    let canonical = rbxm::is_canonical(contents).map_err(|err| format!("can't be read: {err}"))?;
    if canonical {
        Ok(Cow::Borrowed(contents))
    } else {
        rbxm::canonicalize(contents)
            .map(Cow::Owned)
            .map_err(|err| format!("can't be read: {err}"))
    }
}

/// A binary model, read just far enough to tell which values belong to which
/// instance.
struct Model<'a> {
    /// The file's header, which holds how many classes and instances it has.
    header: &'a [u8],
    metadata: BTreeMap<&'a [u8], &'a [u8]>,
    shared_strings: Vec<&'a [u8]>,
    classes: Vec<Class<'a>>,
    instances: Vec<Instance>,
    /// The instances at the top of the model, which is usually just one.
    roots: Vec<usize>,
    /// The contents of the END chunk.
    end: &'a [u8],
    /// Any bytes that follow the END chunk.
    trailing: &'a [u8],
}

/// Every instance of one class, and their properties.
struct Class<'a> {
    name: &'a [u8],
    /// The class's properties, sorted by name.
    columns: Vec<Column<'a>>,
    /// The instances of the class, in the order their values are stored in.
    instances: Vec<usize>,
}

struct Instance {
    class: usize,
    /// Which of the values of its class's properties are this instance's.
    position: usize,
    /// Whether this instance is a service, which the format records apart
    /// from its properties.
    service: bool,
    children: Vec<usize>,
    /// A hash of everything about the instance and its descendants that has
    /// to be identical for them to be the same, so floats and references are
    /// left out. Instances with different fingerprints are never the same.
    fingerprint: u64,
}

/// One property of every instance of a class, which is one PROP chunk.
struct Column<'a> {
    name: &'a [u8],
    value_type: u8,
    /// How many values there are, one for every instance of the class.
    count: usize,
    /// The values, after the chunk's header.
    data: &'a [u8],
    /// Where every value starts in `data`, for types whose values are stored
    /// one after another and vary in size, followed by where the last one
    /// ends.
    starts: Vec<usize>,
    /// The instance every value refers to, for refs.
    refs: Vec<Option<usize>>,
    /// The values of Content properties, which are spread over several lists.
    contents: Vec<ContentValue<'a>>,
}

#[derive(Clone, Copy)]
enum ContentValue<'a> {
    None,
    Uri(&'a [u8]),
    Object(Option<usize>),
}

/// A part of a value that comparing it looks at.
#[derive(Clone, Copy)]
enum Field<'a> {
    /// Bytes that have to be identical.
    Bytes(&'a [u8]),
    /// Up to eight bytes that have to be identical, gathered from an
    /// interleaved array. Unused bytes are zero.
    Small([u8; 8]),
    F32(f32),
    F64(f64),
    /// The instance a reference points to, by its index in its model.
    Ref(Option<usize>),
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(len)
            .filter(|&end| end <= self.data.len())
            .ok_or("is truncated")?;
        let bytes = &self.data[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// A string or a buffer, stored as its length followed by its bytes.
    fn string(&mut self) -> Result<&'a [u8], String> {
        let len = self.u32()? as usize;
        self.bytes(len)
    }

    /// The bytes of `count` values of `width` bytes each.
    fn array(&mut self, count: usize, width: usize) -> Result<&'a [u8], String> {
        self.bytes(count.checked_mul(width).ok_or("is truncated")?)
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.at..];
        self.at = self.data.len();
        rest
    }

    fn finish(&self) -> Result<(), String> {
        if self.at == self.data.len() {
            Ok(())
        } else {
            Err("has unexpected bytes".to_owned())
        }
    }
}

/// The bytes of value `index` of an interleaved array of `count` values, which
/// stores the first byte of every value first, then the second, and so on.
fn gather<const N: usize>(array: &[u8], count: usize, index: usize) -> [u8; N] {
    std::array::from_fn(|byte| array[index + byte * count])
}

fn gather_small<const N: usize>(array: &[u8], count: usize, index: usize) -> [u8; 8] {
    let mut small = [0; 8];
    small[..N].copy_from_slice(&gather::<N>(array, count, index));
    small
}

/// Floats in interleaved arrays are big-endian, with the sign moved to the
/// lowest bit.
fn interleaved_f32(array: &[u8], count: usize, index: usize) -> f32 {
    f32::from_bits(u32::from_be_bytes(gather(array, count, index)).rotate_right(1))
}

fn le_f32(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// Reads an array of referents, which are stored interleaved, with their sign
/// moved to the lowest bit, as the difference from the previous one.
fn referents(array: &[u8], count: usize) -> Vec<i32> {
    let mut last = 0i32;
    (0..count)
        .map(|index| {
            let raw = i32::from_be_bytes(gather(array, count, index));
            let difference = ((raw as u32) >> 1) as i32 ^ -(raw & 1);
            last = last.wrapping_add(difference);
            last
        })
        .collect()
}

/// Reads `count` values that are stored one after another and vary in size,
/// using `read` to read each one, and records where each one starts, followed
/// by where the last one ends.
fn one_after_another<'a>(
    reader: &mut Reader<'a>,
    count: usize,
    starts: &mut Vec<usize>,
    read: impl Fn(&mut Reader<'a>) -> Result<(), String>,
) -> Result<(), String> {
    for _ in 0..count {
        starts.push(reader.at);
        read(reader)?;
    }
    starts.push(reader.at);
    Ok(())
}

/// Reads the rotation of a CFrame: the ID of an axis-aligned rotation, or 0
/// followed by the whole matrix.
fn skip_rotation(reader: &mut Reader) -> Result<(), String> {
    if reader.u8()? == 0 {
        reader.bytes(36)?;
    }
    Ok(())
}

impl<'a> Model<'a> {
    fn parse(contents: &'a [u8]) -> Result<Self, String> {
        let chunks = rbxm::chunks(contents).map_err(|err| format!("can't be read: {err}"))?;

        let mut model = Model {
            header: chunks.header,
            metadata: BTreeMap::new(),
            shared_strings: Vec::new(),
            classes: Vec::new(),
            instances: Vec::new(),
            roots: Vec::new(),
            end: &[],
            trailing: chunks.trailing,
        };

        // Classes by their ID in this file, and instances by their referent.
        let mut class_ids = HashMap::new();
        let mut referents_seen = HashMap::new();
        let mut parents = None;

        for chunk in chunks.chunks {
            let chunk_name = String::from_utf8_lossy(&chunk.name)
                .trim_end_matches('\0')
                .to_owned();

            // Chunks only borrow from the model when they weren't compressed,
            // and nothing is compressed by now.
            let Cow::Borrowed(data) = chunk.data else {
                return Err("has compressed chunks".to_owned());
            };
            if chunk.reserved != [0; 4] {
                return Err(format!("has a {chunk_name} chunk it can't compare"));
            }

            model
                .parse_chunk(
                    &chunk.name,
                    data,
                    &mut class_ids,
                    &mut referents_seen,
                    &mut parents,
                )
                .map_err(|err| format!("{err} in its {chunk_name} chunk"))?;
        }

        for class in &mut model.classes {
            class.columns.sort_by(|a, b| a.name.cmp(b.name));
            if class
                .columns
                .windows(2)
                .any(|pair| pair[0].name == pair[1].name)
            {
                return Err("has a property twice".to_owned());
            }
        }

        let (children, parents) = parents.ok_or("has no hierarchy")?;
        if children.len() != model.instances.len() {
            return Err("has instances that aren't in its hierarchy".to_owned());
        }

        let mut placed = vec![false; model.instances.len()];
        for (child, parent) in children.into_iter().zip(parents) {
            let child = *referents_seen
                .get(&child)
                .ok_or("has an unknown instance in its hierarchy")?;
            if std::mem::replace(&mut placed[child], true) {
                return Err("has an instance in its hierarchy twice".to_owned());
            }

            if parent == -1 {
                model.roots.push(child);
            } else {
                let parent = *referents_seen
                    .get(&parent)
                    .ok_or("has an unknown parent in its hierarchy")?;
                model.instances[parent].children.push(child);
            }
        }

        model.fingerprint_all()?;
        Ok(model)
    }

    fn parse_chunk(
        &mut self,
        name: &[u8; 4],
        data: &'a [u8],
        class_ids: &mut HashMap<u32, usize>,
        referents_seen: &mut HashMap<i32, usize>,
        parents: &mut Option<(Vec<i32>, Vec<i32>)>,
    ) -> Result<(), String> {
        let mut reader = Reader::new(data);

        match name {
            b"META" => {
                let count = reader.u32()?;
                for _ in 0..count {
                    let key = reader.string()?;
                    let value = reader.string()?;
                    self.metadata.insert(key, value);
                }
            }
            b"SSTR" => {
                if reader.u32()? != 0 {
                    return Err("has a version it doesn't know".to_owned());
                }
                let count = reader.u32()?;
                for _ in 0..count {
                    // A hash of the string, which tells nothing that the
                    // string itself doesn't.
                    reader.bytes(16)?;
                    self.shared_strings.push(reader.string()?);
                }
            }
            b"INST" => {
                let id = reader.u32()?;
                let class_name = reader.string()?;
                let service = match reader.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err("has a kind of class it doesn't know".to_owned()),
                };
                let count = reader.u32()? as usize;
                let referents = referents(reader.array(count, 4)?, count);
                if service {
                    // Whether each instance is a service, which they all are
                    // in practice.
                    reader.array(count, 1)?;
                }

                let class = self.classes.len();
                if class_ids.insert(id, class).is_some() {
                    return Err("has a class twice".to_owned());
                }

                let mut instances = Vec::with_capacity(count);
                for (position, referent) in referents.into_iter().enumerate() {
                    let index = self.instances.len();
                    if referents_seen.insert(referent, index).is_some() {
                        return Err("has an instance twice".to_owned());
                    }
                    self.instances.push(Instance {
                        class,
                        position,
                        service,
                        children: Vec::new(),
                        fingerprint: 0,
                    });
                    instances.push(index);
                }

                self.classes.push(Class {
                    name: class_name,
                    columns: Vec::new(),
                    instances,
                });
            }
            b"PROP" => {
                let id = reader.u32()?;
                let property = reader.string()?;
                let value_type = reader.u8()?;
                let class = *class_ids
                    .get(&id)
                    .ok_or("has a property of a class it doesn't have")?;
                let count = self.classes[class].instances.len();

                let column = Column::parse(
                    property,
                    value_type,
                    count,
                    reader.rest(),
                    referents_seen,
                    self.shared_strings.len(),
                )
                .map_err(|err| {
                    format!(
                        "has a value of {}.{} that {err}",
                        String::from_utf8_lossy(self.classes[class].name),
                        String::from_utf8_lossy(property),
                    )
                })?;
                self.classes[class].columns.push(column);
            }
            b"PRNT" => {
                if reader.u8()? != 0 {
                    return Err("has a version it doesn't know".to_owned());
                }
                let count = reader.u32()? as usize;
                let children = referents(reader.array(count, 4)?, count);
                let parents_of = referents(reader.array(count, 4)?, count);
                *parents = Some((children, parents_of));
            }
            b"END\0" => {
                self.end = reader.rest();
            }
            _ => return Err("is a kind of chunk it doesn't know".to_owned()),
        }

        reader.finish()
    }

    /// Computes the fingerprint of every instance, and checks that every
    /// instance descends from a root, which instances parented to each other
    /// in a loop don't.
    fn fingerprint_all(&mut self) -> Result<(), String> {
        // Every instance comes after its parent in this order, so going
        // through it backwards gets to children before their parents.
        let mut order = Vec::with_capacity(self.instances.len());
        let mut to_visit: Vec<(usize, usize)> =
            self.roots.iter().rev().map(|&root| (root, 1)).collect();
        while let Some((index, depth)) = to_visit.pop() {
            if depth > MAX_DEPTH {
                return Err("has instances nested too deeply to compare".to_owned());
            }
            order.push(index);
            to_visit.extend(
                self.instances[index]
                    .children
                    .iter()
                    .map(|&child| (child, depth + 1)),
            );
        }

        if order.len() != self.instances.len() {
            return Err("has instances that are parented to each other".to_owned());
        }

        let mut fields = Vec::new();
        let mut child_fingerprints = Vec::new();
        for &index in order.iter().rev() {
            let instance = &self.instances[index];
            let class = &self.classes[instance.class];

            let mut hasher = DefaultHasher::new();
            class.name.hash(&mut hasher);
            instance.service.hash(&mut hasher);

            for column in &class.columns {
                column.name.hash(&mut hasher);
                column.value_type.hash(&mut hasher);

                fields.clear();
                column.fields(instance.position, &self.shared_strings, &mut fields);
                for field in &fields {
                    match field {
                        Field::Bytes(bytes) => (0u8, bytes).hash(&mut hasher),
                        Field::Small(bytes) => (1u8, bytes).hash(&mut hasher),
                        Field::F32(_) | Field::F64(_) => 2u8.hash(&mut hasher),
                        Field::Ref(target) => (3u8, target.is_some()).hash(&mut hasher),
                    }
                }
            }

            child_fingerprints.clear();
            child_fingerprints.extend(
                instance
                    .children
                    .iter()
                    .map(|&child| self.instances[child].fingerprint),
            );
            // Siblings may be in any order.
            child_fingerprints.sort_unstable();
            child_fingerprints.hash(&mut hasher);

            self.instances[index].fingerprint = hasher.finish();
        }

        Ok(())
    }

    /// The class and name of an instance, which siblings are paired by when
    /// explaining differences.
    fn key_of(&self, index: usize) -> (&'a [u8], &'a [u8]) {
        let instance = &self.instances[index];
        let class = &self.classes[instance.class];

        let name = class
            .columns
            .iter()
            .find(|column| column.name == b"Name" && column.value_type == STRING)
            // Strings are stored with their length first.
            .map_or(&[][..], |column| &column.value(instance.position)[4..]);

        (class.name, name)
    }

    /// The instance's name, or its class if it doesn't have one, for
    /// explaining differences.
    fn name_of(&self, index: usize) -> String {
        let (class, name) = self.key_of(index);
        String::from_utf8_lossy(if name.is_empty() { class } else { name }).into_owned()
    }
}

impl<'a> Column<'a> {
    fn parse(
        name: &'a [u8],
        value_type: u8,
        count: usize,
        data: &'a [u8],
        instances: &HashMap<i32, usize>,
        shared_strings: usize,
    ) -> Result<Self, String> {
        let mut column = Column {
            name,
            value_type,
            count,
            data,
            starts: Vec::new(),
            refs: Vec::new(),
            contents: Vec::new(),
        };
        let mut reader = Reader::new(data);
        let starts = &mut column.starts;

        match value_type {
            BOOL | FACES | AXES => {
                reader.array(count, 1)?;
            }
            INT32 | FLOAT32 | BRICK_COLOR | ENUM => {
                reader.array(count, 4)?;
            }
            FLOAT64 | INT64 | SECURITY_CAPABILITIES | UDIM | VECTOR2 | NUMBER_RANGE => {
                reader.array(count, 8)?;
            }
            UDIM2 | RECT | UNIQUE_ID => {
                reader.array(count, 16)?;
            }
            COLOR3 | VECTOR3 => {
                reader.array(count, 12)?;
            }
            RAY => {
                reader.array(count, 24)?;
            }
            VECTOR3_INT16 => {
                reader.array(count, 6)?;
            }
            COLOR3_UINT8 => {
                reader.array(count, 3)?;
            }
            SHARED_STRING => {
                let array = reader.array(count, 4)?;
                for index in 0..count {
                    if u32::from_be_bytes(gather(array, count, index)) as usize >= shared_strings {
                        return Err("refers to a shared string it doesn't have".to_owned());
                    }
                }
            }
            REF => {
                let array = reader.array(count, 4)?;
                column.refs = referents(array, count)
                    .into_iter()
                    .map(|referent| instances.get(&referent).copied())
                    .collect();
            }
            STRING => {
                one_after_another(&mut reader, count, starts, |reader| {
                    reader.string().map(|_| ())
                })?;
            }
            FONT => {
                one_after_another(&mut reader, count, starts, |reader| {
                    // The family, weight, style, and cached face.
                    reader.string()?;
                    reader.bytes(3)?;
                    reader.string()?;
                    Ok(())
                })?;
            }
            NUMBER_SEQUENCE | COLOR_SEQUENCE => {
                // A time, value, and envelope per keypoint, or a time, three
                // color components, and an unused envelope.
                let keypoint_len = if value_type == NUMBER_SEQUENCE {
                    12
                } else {
                    20
                };
                one_after_another(&mut reader, count, starts, |reader| {
                    let keypoints = reader.u32()? as usize;
                    reader.array(keypoints, keypoint_len)?;
                    Ok(())
                })?;
            }
            PHYSICAL_PROPERTIES => {
                one_after_another(&mut reader, count, starts, |reader| {
                    // Default properties have no floats, and custom ones have
                    // five, or six with acoustic absorption.
                    let floats = match reader.u8()? {
                        0b00 | 0b10 => 0,
                        0b01 => 5,
                        0b11 => 6,
                        _ => return Err("isn't laid out as expected".to_owned()),
                    };
                    reader.array(floats, 4)?;
                    Ok(())
                })?;
            }
            CFRAME => {
                one_after_another(&mut reader, count, starts, skip_rotation)?;
                // The positions follow the rotations.
                reader.array(count, 12)?;
            }
            OPTIONAL_CFRAME => {
                // A CFrame for every value, then whether each value has one.
                if reader.u8()? != CFRAME {
                    return Err("isn't laid out as expected".to_owned());
                }
                one_after_another(&mut reader, count, starts, skip_rotation)?;
                reader.array(count, 12)?;
                if reader.u8()? != BOOL {
                    return Err("isn't laid out as expected".to_owned());
                }
                reader.array(count, 1)?;
            }
            CONTENT => {
                // What kind of content every value is, then the URIs of the
                // values that are URIs, and the instances of the values that
                // are objects, in order.
                let kinds = reader.array(count, 4)?;
                let uri_count = reader.u32()? as usize;
                let mut uris = Vec::new();
                for _ in 0..uri_count {
                    uris.push(reader.string()?);
                }
                let object_count = reader.u32()? as usize;
                let objects = referents(reader.array(object_count, 4)?, object_count);
                // Roblox says these are only for its own use, so they're not
                // something to compare.
                if reader.u32()? != 0 {
                    return Err("refers to instances outside of the model".to_owned());
                }

                let (mut uris, mut objects) = (uris.into_iter(), objects.into_iter());
                for index in 0..count {
                    let raw = i32::from_be_bytes(gather(kinds, count, index));
                    let kind = ((raw as u32) >> 1) as i32 ^ -(raw & 1);
                    column.contents.push(match kind {
                        0 => ContentValue::None,
                        1 => ContentValue::Uri(uris.next().ok_or("is missing a URI")?),
                        2 => ContentValue::Object(
                            instances
                                .get(&objects.next().ok_or("is missing an object")?)
                                .copied(),
                        ),
                        _ => return Err("is a kind of content it doesn't know".to_owned()),
                    });
                }
                if uris.next().is_some() || objects.next().is_some() {
                    return Err("has more content than values".to_owned());
                }
            }
            _ => {
                return Err(format!("is of a type it doesn't know ({value_type:#04x})"));
            }
        }

        reader.finish()?;
        Ok(column)
    }

    /// The bytes of the value at `position`, for values stored one after
    /// another.
    fn value(&self, position: usize) -> &'a [u8] {
        &self.data[self.starts[position]..self.starts[position + 1]]
    }

    /// The `index`th of the interleaved arrays of four byte values that
    /// `data` starts with.
    fn array(&self, data: &'a [u8], index: usize) -> &'a [u8] {
        &data[index * self.count * 4..(index + 1) * self.count * 4]
    }

    /// Lists the parts of the value at `position` that comparing it looks at.
    fn fields(&self, position: usize, shared_strings: &[&'a [u8]], out: &mut Vec<Field<'a>>) {
        let (data, count) = (self.data, self.count);
        let array = |index: usize| self.array(data, index);
        let value = || self.value(position);

        match self.value_type {
            BOOL | FACES | AXES => out.push(Field::Bytes(&data[position..position + 1])),
            INT32 | BRICK_COLOR | ENUM => {
                out.push(Field::Small(gather_small::<4>(data, count, position)));
            }
            SHARED_STRING => {
                let index = u32::from_be_bytes(gather(data, count, position)) as usize;
                out.push(Field::Bytes(shared_strings[index]));
            }
            INT64 | SECURITY_CAPABILITIES => {
                out.push(Field::Small(gather_small::<8>(data, count, position)));
            }
            COLOR3_UINT8 => out.push(Field::Small(gather_small::<3>(data, count, position))),
            VECTOR3_INT16 => out.push(Field::Bytes(&data[position * 6..position * 6 + 6])),
            FLOAT32 => out.push(Field::F32(interleaved_f32(data, count, position))),
            FLOAT64 => {
                let bytes = &data[position * 8..position * 8 + 8];
                out.push(Field::F64(f64::from_le_bytes(bytes.try_into().unwrap())));
            }
            // A scale for each axis, then an offset, which is an integer.
            UDIM => {
                out.push(Field::F32(interleaved_f32(array(0), count, position)));
                out.push(Field::Small(gather_small::<4>(array(1), count, position)));
            }
            UDIM2 => {
                out.push(Field::F32(interleaved_f32(array(0), count, position)));
                out.push(Field::F32(interleaved_f32(array(1), count, position)));
                out.push(Field::Small(gather_small::<4>(array(2), count, position)));
                out.push(Field::Small(gather_small::<4>(array(3), count, position)));
            }
            VECTOR2 | VECTOR3 | COLOR3 | RECT => {
                let components = match self.value_type {
                    VECTOR2 => 2,
                    RECT => 4,
                    _ => 3,
                };
                for component in 0..components {
                    out.push(Field::F32(interleaved_f32(
                        array(component),
                        count,
                        position,
                    )));
                }
            }
            RAY | NUMBER_RANGE => {
                let floats = if self.value_type == RAY { 6 } else { 2 };
                let start = position * floats * 4;
                for float in 0..floats {
                    out.push(Field::F32(le_f32(data, start + float * 4)));
                }
            }
            NUMBER_SEQUENCE | COLOR_SEQUENCE => {
                // How many keypoints there are, then their floats.
                let value = value();
                out.push(Field::Bytes(&value[..4]));
                for at in (4..value.len()).step_by(4) {
                    out.push(Field::F32(le_f32(value, at)));
                }
            }
            PHYSICAL_PROPERTIES => {
                let value = value();
                out.push(Field::Bytes(&value[..1]));
                for at in (1..value.len()).step_by(4) {
                    out.push(Field::F32(le_f32(value, at)));
                }
            }
            CFRAME | OPTIONAL_CFRAME => {
                // The positions are interleaved arrays after the rotations,
                // followed by a marker and whether each value has one, for
                // optional CFrames.
                let positions = &data[self.starts[count]..];

                if self.value_type == OPTIONAL_CFRAME {
                    let has_value = count * 12 + 1 + position;
                    out.push(Field::Bytes(&positions[has_value..has_value + 1]));
                }

                let rotation = value();
                out.push(Field::Bytes(&rotation[..1]));
                for at in (1..rotation.len()).step_by(4) {
                    out.push(Field::F32(le_f32(rotation, at)));
                }

                for axis in 0..3 {
                    let array = self.array(positions, axis);
                    out.push(Field::F32(interleaved_f32(array, count, position)));
                }
            }
            REF => out.push(Field::Ref(self.refs[position])),
            STRING | FONT => out.push(Field::Bytes(value())),
            CONTENT => match self.contents[position] {
                ContentValue::None => out.push(Field::Small([0; 8])),
                ContentValue::Uri(uri) => {
                    out.push(Field::Small([1, 0, 0, 0, 0, 0, 0, 0]));
                    out.push(Field::Bytes(uri));
                }
                ContentValue::Object(target) => {
                    out.push(Field::Small([2, 0, 0, 0, 0, 0, 0, 0]));
                    out.push(Field::Ref(target));
                }
            },
            // A UniqueId only identifies the instance, so it isn't compared.
            UNIQUE_ID => {}
            _ => unreachable!("columns of unknown types are rejected when parsed"),
        }
    }

    fn has_refs(&self) -> bool {
        matches!(self.value_type, REF | CONTENT)
    }
}

fn same_field(a: &Field, b: &Field) -> bool {
    match (a, b) {
        (Field::Bytes(a), Field::Bytes(b)) => a == b,
        (Field::Small(a), Field::Small(b)) => a == b,
        (Field::F32(a), Field::F32(b)) => close_f32(*a, *b),
        (Field::F64(a), Field::F64(b)) => close_f64(*a, *b),
        // Whether references point to the same instance can only be told once
        // every instance is matched up, so that's checked last.
        (Field::Ref(a), Field::Ref(b)) => a.is_some() == b.is_some(),
        _ => false,
    }
}

/// Pairs up every instance of one model with the instance it's the same as in
/// another.
struct Matcher<'m, 'a> {
    a: &'m Model<'a>,
    b: &'m Model<'a>,
    /// The instance of `b` that every instance of `a` is paired with.
    pairs: Vec<Option<usize>>,
    /// The instances of `a` in the order they were paired, so that pairs made
    /// while trying a match that didn't work out can be undone.
    paired: Vec<usize>,
    /// How many more pairs of instances may be tried.
    tries: usize,
    fields_a: Vec<Field<'a>>,
    fields_b: Vec<Field<'a>>,
}

impl<'m, 'a> Matcher<'m, 'a> {
    fn new(a: &'m Model<'a>, b: &'m Model<'a>) -> Self {
        Self {
            a,
            b,
            pairs: vec![None; a.instances.len()],
            paired: Vec::new(),
            tries: a
                .instances
                .len()
                .saturating_mul(TRIES_PER_INSTANCE)
                .saturating_add(1000),
            fields_a: Vec::new(),
            fields_b: Vec::new(),
        }
    }

    /// Whether two instances are the same, apart from their children and the
    /// instances their references point to.
    fn same_instance(&mut self, x: usize, y: usize) -> bool {
        let (a, b) = (self.a, self.b);
        let (x, y) = (&a.instances[x], &b.instances[y]);
        let (class_x, class_y) = (&a.classes[x.class], &b.classes[y.class]);

        if class_x.name != class_y.name
            || x.service != y.service
            || class_x.columns.len() != class_y.columns.len()
        {
            return false;
        }

        class_x
            .columns
            .iter()
            .zip(&class_y.columns)
            .all(|(column_x, column_y)| {
                column_x.name == column_y.name
                    && column_x.value_type == column_y.value_type
                    && self.same_value(column_x, x.position, column_y, y.position)
            })
    }

    /// Whether the values at two positions of the same property are the same,
    /// apart from the instances their references point to.
    fn same_value(
        &mut self,
        column_x: &Column<'a>,
        position_x: usize,
        column_y: &Column<'a>,
        position_y: usize,
    ) -> bool {
        self.fields_a.clear();
        self.fields_b.clear();
        column_x.fields(position_x, &self.a.shared_strings, &mut self.fields_a);
        column_y.fields(position_y, &self.b.shared_strings, &mut self.fields_b);

        self.fields_a.len() == self.fields_b.len()
            && self
                .fields_a
                .iter()
                .zip(&self.fields_b)
                .all(|(a, b)| same_field(a, b))
    }

    /// Pairs two instances and their descendants if they're the same, leaving
    /// the pairs as they were if they're not.
    fn try_pair(&mut self, x: usize, y: usize) -> bool {
        let undo_to = self.paired.len();
        if self.pair_tree(x, y) {
            return true;
        }

        for x in self.paired.drain(undo_to..) {
            self.pairs[x] = None;
        }
        false
    }

    fn pair_tree(&mut self, x: usize, y: usize) -> bool {
        if self.tries == 0 {
            return false;
        }
        self.tries -= 1;

        let (a, b) = (self.a, self.b);
        if a.instances[x].fingerprint != b.instances[y].fingerprint || !self.same_instance(x, y) {
            return false;
        }

        self.pairs[x] = Some(y);
        self.paired.push(x);
        self.match_children(&a.instances[x].children, &b.instances[y].children)
    }

    /// Pairs every one of some siblings in `a` with one of some siblings in
    /// `b`, in whatever order they're in.
    fn match_children(&mut self, xs: &[usize], ys: &[usize]) -> bool {
        if xs.len() != ys.len() {
            return false;
        }

        // Siblings are usually still in the same order.
        let mut unpaired_xs = Vec::new();
        let mut paired_ys = vec![false; ys.len()];
        for (index, (&x, &y)) in xs.iter().zip(ys).enumerate() {
            if self.try_pair(x, y) {
                paired_ys[index] = true;
            } else {
                unpaired_xs.push(x);
            }
        }

        if unpaired_xs.is_empty() {
            return true;
        }

        // The rest can only be the same as siblings with the same fingerprint.
        let mut groups: HashMap<u64, (Vec<usize>, Vec<usize>)> = HashMap::new();
        for x in unpaired_xs {
            let fingerprint = self.a.instances[x].fingerprint;
            groups.entry(fingerprint).or_default().0.push(x);
        }
        for (index, &y) in ys.iter().enumerate() {
            if !paired_ys[index] {
                let fingerprint = self.b.instances[y].fingerprint;
                groups.entry(fingerprint).or_default().1.push(y);
            }
        }

        groups
            .into_values()
            .all(|(xs, ys)| xs.len() == ys.len() && self.match_look_alikes(xs, ys))
    }

    /// Pairs siblings that can only differ in their floats and references.
    fn match_look_alikes(&mut self, mut xs: Vec<usize>, mut ys: Vec<usize>) -> bool {
        // Siblings like these are usually parts that differ in where they are.
        // Sorting both sides by their floats lines up the ones that are the
        // same, so that most of them don't have to be tried against each other.
        if xs.len() > 1 {
            let (a, b) = (self.a, self.b);
            sort_by_floats(a, &mut xs);
            sort_by_floats(b, &mut ys);
        }

        let mut unpaired_xs = Vec::new();
        let mut paired_ys = vec![false; ys.len()];
        for (index, (&x, &y)) in xs.iter().zip(&ys).enumerate() {
            if self.try_pair(x, y) {
                paired_ys[index] = true;
            } else {
                unpaired_xs.push(x);
            }
        }

        let mut unpaired_ys: Vec<usize> = ys
            .into_iter()
            .zip(paired_ys)
            .filter(|(_, paired)| !paired)
            .map(|(y, _)| y)
            .collect();

        for x in unpaired_xs {
            let Some(found) = unpaired_ys.iter().position(|&y| self.try_pair(x, y)) else {
                return false;
            };
            unpaired_ys.swap_remove(found);
        }

        true
    }

    /// Whether the references of every instance point to the instance that's
    /// paired with what its pair's references point to. Only meaningful once
    /// every instance is paired.
    fn references_match(&mut self) -> bool {
        let (a, b) = (self.a, self.b);

        for (x, instance_x) in a.instances.iter().enumerate() {
            let Some(y) = self.pairs[x] else {
                return false;
            };
            let instance_y = &b.instances[y];

            let columns_x = &a.classes[instance_x.class].columns;
            let columns_y = &b.classes[instance_y.class].columns;
            for (column_x, column_y) in columns_x.iter().zip(columns_y) {
                if !column_x.has_refs() {
                    continue;
                }

                self.fields_a.clear();
                self.fields_b.clear();
                column_x.fields(instance_x.position, &a.shared_strings, &mut self.fields_a);
                column_y.fields(instance_y.position, &b.shared_strings, &mut self.fields_b);

                for (field_x, field_y) in self.fields_a.iter().zip(&self.fields_b) {
                    if let (Field::Ref(target_x), Field::Ref(target_y)) = (field_x, field_y) {
                        let paired = target_x.and_then(|target| self.pairs[target]);
                        if paired != *target_y {
                            return false;
                        }
                    }
                }
            }
        }

        true
    }
}

/// Sorts instances by the floats of their properties.
fn sort_by_floats(model: &Model, instances: &mut [usize]) {
    let mut fields = Vec::new();
    let mut keyed: Vec<(Vec<f64>, usize)> = instances
        .iter()
        .map(|&index| {
            let instance = &model.instances[index];
            let mut floats = Vec::new();
            for column in &model.classes[instance.class].columns {
                fields.clear();
                column.fields(instance.position, &model.shared_strings, &mut fields);
                floats.extend(fields.iter().filter_map(|field| match field {
                    Field::F32(value) => Some(f64::from(*value)),
                    Field::F64(value) => Some(*value),
                    _ => None,
                }));
            }
            (floats, index)
        })
        .collect();

    keyed.sort_by(|(a, _), (b, _)| {
        a.iter()
            .zip(b)
            .map(|(a, b)| a.total_cmp(b))
            .find(|ordering| ordering.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for (slot, (_, index)) in instances.iter_mut().zip(keyed) {
        *slot = index;
    }
}

/// Describes where two models that aren't the same differ, as far as it can
/// be told. Siblings are paired by their class and name for this.
fn explain(file: &Model, studio: &Model) -> String {
    let mut matcher = Matcher::new(file, studio);
    explain_siblings(&mut matcher, &file.roots, &studio.roots, "")
        .unwrap_or_else(|| "the instances that references point to differ".to_owned())
}

fn explain_siblings(
    matcher: &mut Matcher,
    xs: &[usize],
    ys: &[usize],
    parent_path: &str,
) -> Option<String> {
    let (a, b) = (matcher.a, matcher.b);
    let path_of = |model: &Model, index: usize| {
        let name = model.name_of(index);
        if parent_path.is_empty() {
            name
        } else {
            format!("{parent_path}.{name}")
        }
    };

    let mut candidates: HashMap<(&[u8], &[u8]), Vec<usize>> = HashMap::new();
    for &y in ys {
        candidates.entry(b.key_of(y)).or_default().push(y);
    }

    let mut pairs = Vec::with_capacity(xs.len());
    for &x in xs {
        let Some(group) = candidates
            .get_mut(&a.key_of(x))
            .filter(|group| !group.is_empty())
        else {
            return Some(format!("{} is only in the file", path_of(a, x)));
        };

        // Prefer a sibling that's the same, so that the difference that's
        // reported is a real one. Only a few are tried, since this is only for
        // telling the user about it.
        let found = group
            .iter()
            .take(8)
            .position(|&y| matcher.try_pair(x, y))
            .unwrap_or(0);
        pairs.push((x, group.remove(found)));
    }

    // Every sibling in the file has one in Studio, so any others are new.
    let paired: HashSet<usize> = pairs.iter().map(|&(_, y)| y).collect();
    if let Some(&y) = ys.iter().find(|y| !paired.contains(y)) {
        return Some(format!("{} is only in Studio", path_of(b, y)));
    }

    for (x, y) in pairs {
        if matcher.pairs[x] == Some(y) {
            continue;
        }

        let path = path_of(a, x);
        if let Some(difference) = explain_instance(matcher, x, y, &path) {
            return Some(difference);
        }

        let children_x = &a.instances[x].children;
        let children_y = &b.instances[y].children;
        if let Some(difference) = explain_siblings(matcher, children_x, children_y, &path) {
            return Some(difference);
        }
    }

    None
}

/// Describes how two instances differ, apart from their children.
fn explain_instance(matcher: &mut Matcher, x: usize, y: usize, path: &str) -> Option<String> {
    let (a, b) = (matcher.a, matcher.b);
    let (instance_x, instance_y) = (&a.instances[x], &b.instances[y]);
    let (class_x, class_y) = (&a.classes[instance_x.class], &b.classes[instance_y.class]);

    if instance_x.service != instance_y.service {
        return Some(format!("{path} is a service in only one of them"));
    }

    let mut columns_y: HashMap<&[u8], &Column> = class_y
        .columns
        .iter()
        .map(|column| (column.name, column))
        .collect();

    for column_x in &class_x.columns {
        let property = format!("{path}.{}", String::from_utf8_lossy(column_x.name));
        let Some(column_y) = columns_y.remove(column_x.name) else {
            return Some(format!("{property} is only in the file"));
        };
        if column_x.value_type != column_y.value_type {
            return Some(format!("{property} has a different type"));
        }

        if !matcher.same_value(column_x, instance_x.position, column_y, instance_y.position) {
            return Some(format!("{property} differs"));
        }
    }

    if let Some(column) = columns_y.into_values().next() {
        return Some(format!(
            "{path}.{} is only in Studio",
            String::from_utf8_lossy(column.name)
        ));
    }

    let (children_x, children_y) = (instance_x.children.len(), instance_y.children.len());
    if children_x != children_y {
        return Some(format!(
            "{path} has {children_x} children in the file but {children_y} in Studio"
        ));
    }

    None
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
            CFrame, Color3, Content, CustomPhysicalProperties, Matrix3, NumberSequence,
            NumberSequenceKeypoint, PhysicalProperties, Ref, UDim, UDim2, UniqueId, Variant,
            Vector3,
        },
        InstanceBuilder, WeakDom,
    };

    /// The next float up, the smallest possible change.
    fn bump(value: f32) -> f32 {
        f32::from_bits(value.to_bits() + 1)
    }

    fn same(a: &[u8], b: &[u8]) -> bool {
        compare(a, b).is_ok()
    }

    fn serialize(dom: &WeakDom, root: Ref) -> Vec<u8> {
        let mut output = Vec::new();
        rbx_binary::to_writer(&mut output, dom, &[root]).unwrap();
        output
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
        serialize(&dom, model)
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

    fn at(x: f32) -> CFrame {
        CFrame::new(Vector3::new(x, 1.0, 2.0), Matrix3::identity())
    }

    /// A model holding a part for each of the given names and x positions,
    /// in that order.
    fn parts(parts: &[(&str, f32)]) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(
            dom.root_ref(),
            InstanceBuilder::new("Model").with_name("Tree"),
        );
        for &(name, x) in parts {
            dom.insert(
                model,
                InstanceBuilder::new("Part")
                    .with_name(name)
                    .with_property("CFrame", at(x)),
            );
        }
        serialize(&dom, model)
    }

    /// Puts a binary model back together from its chunks, after `edit` had a
    /// go at changing them.
    fn edit_chunks(contents: &[u8], edit: impl Fn(&[u8; 4], Vec<u8>) -> Vec<u8>) -> Vec<u8> {
        let canonical = rbxm::canonicalize(contents).unwrap();
        let chunks = rbxm::chunks(&canonical).unwrap();

        let mut output = chunks.header.to_vec();
        for chunk in &chunks.chunks {
            let data = edit(&chunk.name, chunk.data.to_vec());
            output.extend_from_slice(&chunk.name);
            output.extend_from_slice(&0u32.to_le_bytes());
            output.extend_from_slice(&(data.len() as u32).to_le_bytes());
            output.extend_from_slice(&chunk.reserved);
            output.extend_from_slice(&data);
        }
        output.extend_from_slice(chunks.trailing);
        output
    }

    #[test]
    fn identical_models_are_the_same() {
        let a = model(vec![("Transparency", 0.25f32.into())]);
        assert!(same(&a, &a.clone()));
    }

    #[test]
    fn rounding_in_rotations_is_ignored() {
        let a = model(vec![("CFrame", rotated(0.887).into())]);
        let b = model(vec![("CFrame", rotated(bump(bump(0.887))).into())]);
        assert_ne!(a, b);
        assert!(same(&a, &b));
    }

    #[test]
    fn real_changes_are_not_ignored() {
        let a = model(vec![("CFrame", rotated(0.887).into())]);
        let b = model(vec![("CFrame", rotated(0.888).into())]);
        assert!(!same(&a, &b));

        // Even far from the origin, where floats are coarse, a move of a
        // hundredth of a stud is a real change.
        let a = model(vec![("CFrame", at(10_000.0).into())]);
        let b = model(vec![("CFrame", at(10_000.01).into())]);
        assert!(!same(&a, &b));
    }

    #[test]
    fn axis_aligned_rotations_have_to_stay_axis_aligned() {
        // rbx_binary writes rotations this close to an axis-aligned one as
        // that rotation's ID, so the whole matrix is put in by hand.
        let aligned = model(vec![("CFrame", at(1.0).into())]);
        let almost = edit_chunks(&aligned, |name, data| {
            let header = 4 + 4 + "CFrame".len() + 1;
            if name != b"PROP" || data.get(8..8 + "CFrame".len()) != Some(b"CFrame") {
                return data;
            }

            let mut edited = data[..header].to_vec();
            edited.push(0);
            for value in [bump(1.0), 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
                edited.extend_from_slice(&value.to_le_bytes());
            }
            edited.extend_from_slice(&data[header + 1..]);
            edited
        });

        assert_ne!(rbxm::canonicalize(&aligned).unwrap(), almost);
        assert!(same(&almost, &almost.clone()));
        assert!(!same(&aligned, &almost));
    }

    #[test]
    fn rounding_near_zero_is_ignored() {
        let a = model(vec![("Transparency", 0.0f32.into())]);
        let b = model(vec![("Transparency", (-3e-8f32).into())]);
        assert!(same(&a, &b));
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
        assert!(same(&a, &b));
        assert!(!same(&a, &model(properties(0.75))));
    }

    #[test]
    fn rounding_in_pivots_is_ignored() {
        // Every model has a pivot, which is an optional CFrame.
        let pivot = |pivot: Option<CFrame>| {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let model = dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("Model")
                    .with_name("Arm")
                    .with_property("WorldPivotData", pivot),
            );
            serialize(&dom, model)
        };

        let a = pivot(Some(rotated(0.887)));
        let b = pivot(Some(rotated(bump(0.887))));
        assert_ne!(a, b);
        assert!(same(&a, &b));
        assert!(!same(&a, &pivot(Some(rotated(0.888)))));
        assert!(!same(&a, &pivot(None)));
    }

    #[test]
    fn integers_have_to_match_exactly() {
        let udim2 = |offset: i32| UDim2::new(UDim::new(0.5, offset), UDim::new(0.5, 0)).into();
        let a = model(vec![("UDim2Test", udim2(10))]);
        let b = model(vec![("UDim2Test", udim2(11))]);
        assert!(!same(&a, &b));
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

        assert!(!same(&a, &serialize(&renamed, arm)));
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

        assert!(!same(&a, &serialize(&dom, arm)));
    }

    #[test]
    fn invalid_models_are_never_the_same() {
        let a = model(vec![]);
        assert!(!same(&a, b"not a model"));
        assert!(!same(b"not a model", b"not a model"));
    }

    #[test]
    fn siblings_in_another_order_are_the_same() {
        let a = parts(&[("Trunk", 0.0), ("Branch", 1.0), ("Leaf", 2.0)]);
        let b = parts(&[("Leaf", 2.0), ("Trunk", 0.0), ("Branch", 1.0)]);
        assert_ne!(a, b);
        assert!(same(&a, &b));

        // Moving siblings doesn't hide changes made to them.
        let c = parts(&[("Leaf", 2.5), ("Trunk", 0.0), ("Branch", 1.0)]);
        assert!(!same(&a, &c));
    }

    #[test]
    fn look_alike_siblings_in_another_order_are_the_same() {
        // A pile of parts with the same name that only differ in where they
        // are, rounded differently and in a completely different order.
        let positions: Vec<f32> = (0..200).map(|index| index as f32 * 0.37).collect();
        let a: Vec<(&str, f32)> = positions.iter().map(|&x| ("Part", x)).collect();
        let b: Vec<(&str, f32)> = positions.iter().rev().map(|&x| ("Part", bump(x))).collect();
        assert!(same(&parts(&a), &parts(&b)));

        // One of them moving is still a change.
        let mut c = a.clone();
        c[17].1 += 0.5;
        assert!(!same(&parts(&a), &parts(&c)));
    }

    #[test]
    fn moving_an_instance_to_another_parent_is_a_change() {
        let tree = |leaf_in_first: bool| {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let model = dom.insert(dom.root_ref(), InstanceBuilder::new("Model"));
            let first = dom.insert(model, InstanceBuilder::new("Folder").with_name("Box"));
            let second = dom.insert(model, InstanceBuilder::new("Folder").with_name("Box"));
            let parent = if leaf_in_first { first } else { second };
            dom.insert(parent, InstanceBuilder::new("Part").with_name("Leaf"));
            serialize(&dom, model)
        };

        // The two folders look the same, so this is the same model.
        assert!(same(&tree(true), &tree(false)));

        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(dom.root_ref(), InstanceBuilder::new("Model"));
        let first = dom.insert(model, InstanceBuilder::new("Folder").with_name("Box"));
        dom.insert(model, InstanceBuilder::new("Folder").with_name("Crate"));
        dom.insert(first, InstanceBuilder::new("Part").with_name("Leaf"));
        let in_box = serialize(&dom, model);

        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(dom.root_ref(), InstanceBuilder::new("Model"));
        dom.insert(model, InstanceBuilder::new("Folder").with_name("Box"));
        let second = dom.insert(model, InstanceBuilder::new("Folder").with_name("Crate"));
        dom.insert(second, InstanceBuilder::new("Part").with_name("Leaf"));
        let in_crate = serialize(&dom, model);

        assert!(!same(&in_box, &in_crate));
    }

    /// A model with a weld between two of its parts, which are listed in the
    /// given order.
    fn welded(order: [&str; 3], part1: &str) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let model = dom.insert(dom.root_ref(), InstanceBuilder::new("Model"));
        let mut refs = HashMap::new();
        for name in order {
            refs.insert(
                name,
                dom.insert(model, InstanceBuilder::new("Part").with_name(name)),
            );
        }
        dom.insert(
            model,
            InstanceBuilder::new("WeldConstraint")
                .with_property("Part0", refs["Handle"])
                .with_property("Part1", refs[part1]),
        );
        serialize(&dom, model)
    }

    #[test]
    fn references_have_to_point_to_the_same_instance() {
        let a = welded(["Handle", "Blade", "Guard"], "Blade");
        let b = welded(["Guard", "Blade", "Handle"], "Blade");
        assert!(same(&a, &b));

        let c = welded(["Handle", "Blade", "Guard"], "Guard");
        assert!(!same(&a, &c));
        assert!(!same(&b, &c));
    }

    #[test]
    fn content_references_have_to_point_to_the_same_instance() {
        let decal = |order: [&str; 2], target: &str| {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let model = dom.insert(dom.root_ref(), InstanceBuilder::new("Model"));
            let mut refs = HashMap::new();
            for name in order {
                refs.insert(
                    name,
                    dom.insert(model, InstanceBuilder::new("Folder").with_name(name)),
                );
            }
            dom.insert(
                model,
                InstanceBuilder::new("Decal")
                    .with_property("ContentTest", Content::from_referent(refs[target])),
            );
            serialize(&dom, model)
        };

        let a = decal(["Front", "Back"], "Front");
        assert!(same(&a, &decal(["Back", "Front"], "Front")));
        assert!(!same(&a, &decal(["Front", "Back"], "Back")));
    }

    #[test]
    fn unique_ids_are_ignored() {
        let with_id = |random: i64| model(vec![("UniqueId", UniqueId::new(1, 2, random).into())]);

        let a = with_id(12345);
        let b = with_id(67890);
        assert_ne!(a, b);
        assert!(same(&a, &b));
        assert!(!same(&a, &model(vec![])));
    }

    #[test]
    fn deeply_nested_models_are_compared_safely() {
        let chain = |depth: usize| {
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let root = dom.insert(dom.root_ref(), InstanceBuilder::new("Folder"));
            let mut parent = root;
            for _ in 1..depth {
                parent = dom.insert(parent, InstanceBuilder::new("Folder"));
            }
            serialize(&dom, root)
        };

        let shallow = chain(MAX_DEPTH);
        assert!(same(&shallow, &shallow.clone()));

        let deep = chain(MAX_DEPTH * 4);
        assert!(!same(&deep, &deep.clone()));
    }

    #[test]
    fn differences_say_where_they_are() {
        let a = model(vec![("Transparency", 0.25f32.into())]);
        let b = model(vec![("Transparency", 0.5f32.into())]);
        assert_eq!(
            compare(&a, &b).unwrap_err().to_string(),
            "Arm.Palm.Transparency differs"
        );

        let reordered = parts(&[("Trunk", 0.0), ("Branch", 1.0)]);
        let moved = parts(&[("Branch", 1.5), ("Trunk", 0.0)]);
        assert_eq!(
            compare(&reordered, &moved).unwrap_err().to_string(),
            "Tree.Branch.CFrame differs"
        );

        let fewer = parts(&[("Trunk", 0.0)]);
        assert_eq!(
            compare(&reordered, &fewer).unwrap_err().to_string(),
            "they have a different number of instances or classes"
        );

        let renamed = parts(&[("Trunk", 0.0), ("Twig", 1.0)]);
        assert_eq!(
            compare(&reordered, &renamed).unwrap_err().to_string(),
            "Tree.Branch is only in the file"
        );

        assert!(compare(&a, b"not a model")
            .unwrap_err()
            .to_string()
            .starts_with("Studio's version can't be read"));
    }
}

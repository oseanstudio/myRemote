use std::io::Cursor;

use plist::stream::BinaryReader;
use plist::{Dictionary, Value};
use rand::{rngs::OsRng, RngCore};

use crate::{keyed_archive, opack};
use keyed_archive::{class, dictionary, uid};

pub(crate) struct KeyboardSession {
    pub field: u64,
    pub id: [u8; 16],
}

pub(crate) struct TextEdit {
    pub target: [u8; 16],
    pub backspaces: u8,
    pub deletes: u8,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardEditError {
    Envelope = 0,
    Archive,
    Operation,
    Target,
    Output,
    NoEdit,
}

impl KeyboardSession {
    pub fn new(field: u64) -> Self {
        let mut id = [0; 16];
        OsRng.fill_bytes(&mut id);
        Self {
            field,
            id: *uuid::Builder::from_random_bytes(id).into_uuid().as_bytes(),
        }
    }

    pub fn focus_payload(&self) -> opack::Value {
        let state = dictionary([("docSt", uid(4)), ("$class", uid(5))]);
        let context = dictionary([
            ("contextBeforeInput", uid(7)),
            ("selectedText", uid(7)),
            ("contextAfterInput", uid(7)),
            ("$class", uid(6)),
        ]);
        let objects = vec![
            Value::String("$null".into()),
            dictionary([
                ("NS.uuidbytes", Value::Data(self.id.to_vec())),
                ("$class", uid(2)),
            ]),
            class("NSUUID"),
            state,
            context,
            class("RTIDocumentState"),
            class("TIDocumentState"),
            Value::String(String::new()),
            dictionary([("$class", uid(9))]),
            class("RTIDocumentTraits"),
        ];
        // Describe the focus without exporting any desktop text or field metadata.
        let bytes = keyed_archive::encode(
            "RTIKeyedArchiver",
            dictionary([
                ("sessionUUID", uid(1)),
                ("documentState", uid(3)),
                ("documentTraits", uid(8)),
            ]),
            objects,
        );
        opack::Value::dict(vec![opack::Value::kv("_tiD", opack::Value::Bytes(bytes))])
    }

    pub fn text_edit(
        content: &opack::Value,
        message: &opack::Value,
    ) -> Result<TextEdit, KeyboardEditError> {
        use KeyboardEditError::*;

        // The authenticated message identifier and recognized archive
        // structure identify the operation across RTI version numbers.
        let bytes = content
            .get("_tiD")
            .or_else(|| message.get("_tiD"))
            .unwrap_or(content)
            .as_bytes()
            .ok_or(Envelope)?;
        if bytes.len() > 48 * 1024 || !bytes.starts_with(b"bplist00") {
            return Err(Archive);
        }
        // Bound the expanded event stream as binary-plist references can reuse
        // objects many times. Decode data only, with no Objective-C unarchiving.
        let archive = Value::from_events(BinaryReader::new(Cursor::new(bytes)).take(4096))
            .map_err(|_| Archive)?;
        let root = archive.as_dictionary().ok_or(Archive)?;
        let objects = root
            .get("$objects")
            .and_then(Value::as_array)
            .ok_or(Archive)?;
        let top = root
            .get("$top")
            .and_then(Value::as_dictionary)
            .ok_or(Archive)?;
        let root = ["textOperations", "inputOperation", "root"]
            .into_iter()
            .find_map(|key| property(objects, top, key).and_then(Value::as_dictionary))
            .or_else(|| {
                top.values().find_map(|value| {
                    let dict = resolve(objects, value)?.as_dictionary()?;
                    let class = property(objects, dict, "$class")?.as_dictionary()?;
                    matches!(
                        class.get("$classname")?.as_string()?,
                        "RTITextOperations" | "RTIInputOperation"
                    )
                    .then_some(dict)
                })
            })
            .ok_or(Operation)?;
        let operations = property(objects, root, "textOperations")
            .and_then(Value::as_dictionary)
            .unwrap_or(root);
        let target_reference = [operations, root, top]
            .into_iter()
            .find_map(|dict| {
                property(objects, dict, "targetSessionUUID")
                    .or_else(|| property(objects, dict, "sessionUUID"))
            })
            .ok_or(Target)?;
        let target_value = target_reference
            .as_dictionary()
            .and_then(|dict| property(objects, dict, "NS.uuidbytes"))
            .unwrap_or(target_reference);
        let target = target_value
            .as_data()
            .and_then(|bytes| bytes.try_into().ok())
            .or_else(|| {
                Some(
                    *uuid::Uuid::parse_str(target_value.as_string()?)
                        .ok()?
                        .as_bytes(),
                )
            })
            .ok_or(Target)?;
        let output = property(objects, operations, "keyboardOutput")
            .and_then(Value::as_dictionary)
            .unwrap_or(operations);
        // A text assertion refers to the phone's empty synthetic document.
        // Applying it to the real field could erase text typed elsewhere.
        let mut backspaces = deletion_count(objects, output, "deletionCount").ok_or(Output)?;
        let mut deletes = deletion_count(objects, output, "forwardDeletionCount").ok_or(Output)?;
        let mut text = [
            "insertionText",
            "textToCommit",
            "insertionTextAfterSelection",
        ]
        .into_iter()
        .filter_map(|key| read_string(objects, property(objects, output, key)?))
        .find(|text| !text.is_empty())
        .or_else(|| {
            read_string(
                objects,
                property(objects, operations, "attributedInsertionText")?,
            )
        })
        .unwrap_or("");
        if backspaces == 0 && deletes == 0 && text.is_empty() {
            match property(objects, operations, "editingActionSelector").and_then(Value::as_string)
            {
                Some("deleteBackward:") => backspaces = 1,
                Some("deleteForward:") => deletes = 1,
                Some("insertNewline:") => text = "\n",
                _ => return Err(NoEdit),
            }
        }
        if text.len() > 8 * 1024 {
            return Err(Output);
        }
        Ok(TextEdit {
            target,
            backspaces,
            deletes,
            text: text.to_owned(),
        })
    }
}

fn read_string<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a str> {
    let mut text = value;
    for _ in 0..4 {
        let Some(attrs) = text.as_dictionary() else {
            return text.as_string();
        };
        text = property(objects, attrs, "NSString")
            .or_else(|| property(objects, attrs, "NS.string"))?;
    }
    None
}

fn deletion_count(objects: &[Value], output: &Dictionary, key: &str) -> Option<u8> {
    match property(objects, output, key) {
        None => Some(0),
        Some(value) => {
            let count = value.as_unsigned_integer().or_else(|| {
                property(objects, value.as_dictionary()?, "NS.intval")?.as_unsigned_integer()
            })?;
            (count <= 64).then_some(count as u8)
        }
    }
}

fn property<'a>(objects: &'a [Value], dict: &'a Dictionary, key: &str) -> Option<&'a Value> {
    if let Some(value) = dict.get(key) {
        return resolve(objects, value);
    }
    // NSKeyedArchiver can encode NSDictionary entries as parallel arrays.
    let keys = resolve(objects, dict.get("NS.keys")?)?.as_array()?;
    let values = resolve(objects, dict.get("NS.objects")?)?.as_array()?;
    keys.iter().zip(values).find_map(|(name, value)| {
        let name = resolve(objects, name)?.as_string()?;
        if name == key {
            resolve(objects, value)
        } else {
            None
        }
    })
}

fn resolve<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a Value> {
    match value.as_uid() {
        Some(reference) if reference.get() == 0 => None,
        Some(reference) => objects.get(usize::try_from(reference.get()).ok()?),
        None => Some(value),
    }
}

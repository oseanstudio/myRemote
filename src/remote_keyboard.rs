use std::io::Cursor;

use plist::stream::BinaryReader;
use plist::{Dictionary, Value};
use rand::{rngs::OsRng, RngCore};

use crate::{keyed_archive, opack};
use keyed_archive::{class, dictionary, uid};

pub(crate) struct KeyboardSession {
    pub field: u64,
    id: [u8; 16],
}

pub(crate) struct TextEdit {
    pub backspaces: u8,
    pub deletes: u8,
    pub text: String,
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

    pub fn text_edit(&self, data: &opack::Value) -> Option<TextEdit> {
        let bytes = data.get("_tiD")?.as_bytes()?;
        if bytes.len() > 16 * 1024 || data.get("_tiV")?.as_int()? != 1 {
            return None;
        }
        // Bound the expanded event stream as binary-plist references can reuse
        // objects many times. Decode data only, with no Objective-C unarchiving.
        let archive = Value::from_events(BinaryReader::new(Cursor::new(bytes)).take(512))
            .inspect_err(|_| tracing::debug!("Ignoring an invalid keyboard archive"))
            .ok()?;
        let root = archive.as_dictionary()?;
        let objects = root.get("$objects")?.as_array()?;
        let top = root.get("$top")?.as_dictionary()?;
        let operations = resolve(objects, top.get("textOperations")?)?.as_dictionary()?;
        let target = resolve(objects, operations.get("targetSessionUUID")?)?.as_dictionary()?;
        if resolve(objects, target.get("NS.uuidbytes")?)?.as_data()? != self.id {
            return None;
        }
        let output = resolve(objects, operations.get("keyboardOutput")?)?.as_dictionary()?;
        // A text assertion refers to the phone's empty synthetic document.
        // Applying it to the real field could erase text typed elsewhere.
        let backspaces = deletion_count(objects, output, "deletionCount")?;
        let deletes = deletion_count(objects, output, "forwardDeletionCount")?;
        let text = output
            .get("insertionText")
            .and_then(|value| resolve(objects, value))
            .and_then(Value::as_string)
            .unwrap_or("");
        if text.len() > 8 * 1024 || backspaces == 0 && deletes == 0 && text.is_empty() {
            return None;
        }
        Some(TextEdit {
            backspaces,
            deletes,
            text: text.to_owned(),
        })
    }
}

fn deletion_count(objects: &[Value], output: &Dictionary, key: &str) -> Option<u8> {
    match output.get(key) {
        None => Some(0),
        Some(value) => {
            let count = resolve(objects, value)?.as_unsigned_integer()?;
            (count <= 64).then_some(count as u8)
        }
    }
}

fn resolve<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a Value> {
    match value.as_uid() {
        Some(reference) => objects.get(usize::try_from(reference.get()).ok()?),
        None => Some(value),
    }
}

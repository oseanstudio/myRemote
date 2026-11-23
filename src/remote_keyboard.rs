use std::io::Cursor;

use plist::stream::BinaryReader;
use plist::{Dictionary, Value};
use rand::{rngs::OsRng, RngCore};
use unicode_segmentation::UnicodeSegmentation;

use crate::{keyed_archive, opack};
use keyed_archive::{class, dictionary, uid};

pub(crate) struct KeyboardSession {
    pub field: u64,
    pub id: [u8; 16],
}

pub(crate) struct TextEdit {
    pub target: Option<[u8; 16]>,
    backspaces: u8,
    deletes: u8,
    insertion: String,
    commit: Option<String>,
    pub marked: Option<String>,
    unmark: bool,
}

pub(crate) struct KeyboardField {
    pub id: u64,
    marked: String,
}

pub(crate) struct KeyboardInput {
    pub backspaces: u8,
    pub deletes: u8,
    pub text: String,
}

impl KeyboardField {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            marked: String::new(),
        }
    }

    pub fn apply(&mut self, edit: TextEdit) -> Option<KeyboardInput> {
        let mut backspaces = edit.backspaces;
        let mut text = edit.insertion;
        // Composed text lives on the phone until committed. A delete inside
        // that text must not erase characters that were already on the Mac.
        if !self.marked.is_empty() && edit.commit.is_none() && !edit.unmark && backspaces != 0 {
            let mut graphemes = self.marked.grapheme_indices(true).rev();
            let mut end = self.marked.len();
            while backspaces != 0 {
                let Some((index, _)) = graphemes.next() else {
                    break;
                };
                end = index;
                backspaces -= 1;
            }
            self.marked.truncate(end);
        }
        if let Some(committed) = edit.commit {
            text.insert_str(0, &committed);
            self.marked.clear();
        } else if edit.unmark
            || (edit.marked.is_none() && !text.is_empty() && text.chars().all(char::is_whitespace))
        {
            text.insert_str(0, &std::mem::take(&mut self.marked));
        } else if !text.is_empty() {
            // A direct insertion replaces the phone's unposted marked text.
            self.marked.clear();
        }
        if let Some(marked) = edit.marked {
            self.marked = marked;
        }
        (backspaces != 0 || edit.deletes != 0 || !text.is_empty()).then_some(KeyboardInput {
            backspaces,
            deletes: edit.deletes,
            text,
        })
    }
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
        let state = dictionary([("docSt", uid(3)), ("$class", uid(4))]);
        let context = dictionary([
            ("contextBeforeInput", uid(6)),
            ("selectedText", uid(6)),
            ("contextAfterInput", uid(6)),
            ("$class", uid(5)),
        ]);
        let objects = vec![
            Value::String("$null".into()),
            // RTIInputSystemDataPayload encodes the top-level sessionUUID as
            // 16 raw bytes. An NSUUID object is used for targetSessionUUID
            // inside incoming RTITextOperations, which has a different decoder.
            Value::Data(self.id.to_vec()),
            state,
            context,
            class("RTIDocumentState"),
            class("TIDocumentState"),
            Value::String(String::new()),
            dictionary([("$class", uid(8))]),
            class("RTIDocumentTraits"),
        ];
        // Describe the focus without exporting any desktop text or field metadata.
        let bytes = keyed_archive::encode(
            "RTIKeyedArchiver",
            dictionary([
                ("sessionUUID", uid(1)),
                ("documentState", uid(2)),
                ("documentTraits", uid(7)),
            ]),
            objects,
        );
        opack::Value::dict(vec![
            opack::Value::kv("_tiV", opack::Value::Int(1)),
            opack::Value::kv("_tiD", opack::Value::Bytes(bytes)),
        ])
    }

    pub fn text_edit(
        content: &opack::Value,
        message: &opack::Value,
        version: Option<u64>,
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
        let archiver = root.get("$archiver").and_then(Value::as_string);
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
                    matches!(
                        class_name(objects, dict)?,
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
            .flat_map(|dict| {
                ["targetSessionUUID", "sessionUUID"]
                    .into_iter()
                    .filter_map(move |key| keyed_value(objects, dict, key))
            })
            .find(|value| value.as_uid().is_none_or(|uid| uid.get() != 0));
        let target = match target_reference {
            Some(reference) => Some(read_uuid(objects, reference).ok_or(Target)?),
            None if version == Some(1)
                && archiver == Some("RTIKeyedArchiver")
                && class_name(objects, operations) == Some("RTITextOperations") =>
            {
                None
            }
            None => return Err(Target),
        };
        let output = property(objects, operations, "keyboardOutput")
            .and_then(Value::as_dictionary)
            .unwrap_or(operations);
        // A text assertion refers to the phone's empty synthetic document.
        // Applying it to the real field could erase text typed elsewhere.
        let mut backspaces = deletion_count(objects, output, "deletionCount").ok_or(Output)?;
        let mut deletes = deletion_count(objects, output, "forwardDeletionCount").ok_or(Output)?;
        let mut insertion = ["insertionText", "insertionTextAfterSelection"]
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
        let commit =
            property(objects, output, "textToCommit").and_then(|value| read_string(objects, value));
        let marked = [
            ("styledIntermediateText", "styledDisplayString"),
            ("intermediateText", "displayString"),
        ]
        .into_iter()
        .find_map(|(operation, display)| {
            let mark = property(objects, operations, operation)?.as_dictionary()?;
            read_string(objects, property(objects, mark, display)?)
        });
        let mut unmark = property(objects, output, "unmarkIfNecessary").and_then(Value::as_boolean)
            == Some(true);
        if backspaces == 0
            && deletes == 0
            && insertion.is_empty()
            && commit.is_none()
            && marked.is_none()
            && !unmark
        {
            match property(objects, operations, "editingActionSelector").and_then(Value::as_string)
            {
                Some("deleteBackward:") => backspaces = 1,
                Some("deleteForward:") => deletes = 1,
                Some("insertNewline:") => insertion = "\n",
                Some("unmarkText" | "unmarkText:") => unmark = true,
                _ => return Err(NoEdit),
            }
        }
        if insertion.len() + commit.map_or(0, str::len) > 8 * 1024
            || marked.is_some_and(|text| text.len() > 8 * 1024)
        {
            return Err(Output);
        }
        Ok(TextEdit {
            target,
            backspaces,
            deletes,
            insertion: insertion.to_owned(),
            commit: commit.map(str::to_owned),
            marked: marked.map(str::to_owned),
            unmark,
        })
    }
}

fn class_name<'a>(objects: &'a [Value], dict: &'a Dictionary) -> Option<&'a str> {
    property(objects, dict, "$class")?
        .as_dictionary()?
        .get("$classname")?
        .as_string()
}

fn read_uuid(objects: &[Value], reference: &Value) -> Option<[u8; 16]> {
    let target = resolve(objects, reference)?;
    let uuid = target
        .as_dictionary()
        .and_then(|dict| property(objects, dict, "NS.uuidbytes"))
        .unwrap_or(target);
    uuid.as_data()
        .and_then(|bytes| bytes.try_into().ok())
        .or_else(|| Some(*uuid::Uuid::parse_str(uuid.as_string()?).ok()?.as_bytes()))
}

fn read_string<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a str> {
    let mut text = value;
    for _ in 0..4 {
        let Some(attrs) = text.as_dictionary() else {
            return text.as_string();
        };
        if let Some(bytes) = property(objects, attrs, "NS.bytes").and_then(Value::as_data) {
            return std::str::from_utf8(bytes).ok();
        }
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
    resolve(objects, keyed_value(objects, dict, key)?)
}

fn keyed_value<'a>(objects: &'a [Value], dict: &'a Dictionary, key: &str) -> Option<&'a Value> {
    if let Some(value) = dict.get(key) {
        return Some(value);
    }
    // NSKeyedArchiver can encode NSDictionary entries as parallel arrays.
    let keys = resolve(objects, dict.get("NS.keys")?)?.as_array()?;
    let values = resolve(objects, dict.get("NS.objects")?)?.as_array()?;
    keys.iter().zip(values).find_map(|(name, value)| {
        let name = resolve(objects, name)?.as_string()?;
        (name == key).then_some(value)
    })
}

fn resolve<'a>(objects: &'a [Value], value: &'a Value) -> Option<&'a Value> {
    match value.as_uid() {
        Some(reference) if reference.get() == 0 => None,
        Some(reference) => objects.get(usize::try_from(reference.get()).ok()?),
        None => Some(value),
    }
}

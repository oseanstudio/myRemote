use plist::{Uid, Value};

pub(crate) fn uid(index: u64) -> Value {
    Value::Uid(Uid::new(index))
}

pub(crate) fn dictionary<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

pub(crate) fn class(name: &str) -> Value {
    dictionary([
        ("$classname", Value::String(name.into())),
        (
            "$classes",
            Value::Array(vec![
                Value::String(name.into()),
                Value::String("NSObject".into()),
            ]),
        ),
    ])
}

pub(crate) fn encode(archiver: &str, top: Value, objects: Vec<Value>) -> Vec<u8> {
    let archive = dictionary([
        ("$version", Value::Integer(100000.into())),
        ("$archiver", Value::String(archiver.into())),
        ("$top", top),
        ("$objects", Value::Array(objects)),
    ]);
    let mut bytes = Vec::new();
    archive
        .to_writer_binary(&mut bytes)
        .expect("generated keyed archive is serializable");
    bytes
}

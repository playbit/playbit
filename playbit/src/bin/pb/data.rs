//! CLI storage diagnostics share guest data semantics and explicit tool authority.
use playbit::{
    engine::{Request, Result, reply::Reply, tool_client::DataContext},
    runtime::{
        host::{DataCall, DataOperation, DataResult, FieldChange},
        store_bridge,
    },
    store::{Id, object_contract},
};
use std::{collections::BTreeMap, fs, io::Read, path::Path};

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(playbit_api::abi::syscall::MAX_DATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > playbit_api::abi::syscall::MAX_DATA_BYTES {
        return Err("Data input exceeds the 1 MiB data limit".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub(super) fn run(
    args: &[String],
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<serde_json::Value> {
    let address = super::entry_address(args)?;
    let volume = address.volume;
    let entry_id = *address.entry.as_bytes();
    let operation = match args[0].as_str() {
        "schema" if args.len() == 4 => DataOperation::SchemaEstablish {
            entry_id,            schema: store_bridge::from_schema(read_json(Path::new(&args[3]))?),
        },
        "object-create" if args.len() == 5 => {
            let fields: BTreeMap<Id, Option<Vec<object_contract::Value>>> = read_json(Path::new(&args[4]))?;
            DataOperation::Create {
                entry_id,                type_id: *Id::parse(&args[3])?.as_bytes(),
                fields: fields.into_iter().map(|(id, values)| FieldChange {
                    field_id: *id.as_bytes(),
                    values: values.map(|values| values.into_iter().map(store_bridge::from_owned_value).collect()),
                }).collect(),
            }
        }
        "objects" if (4..=5).contains(&args.len()) => return objects(address, Id::parse(&args[3])?, args.get(4).map(|id| Id::parse(id)).transpose()?, call),
        _ => return Err("expected pb schema VOLUME DATABASE JSON_FILE, object-create VOLUME DATABASE TYPE JSON_FILE, or objects VOLUME DATABASE TYPE [AFTER]".into()),
    };
    let mut context = DataContext::open(None, call)?;
    let result = context.execute(DataCall { volume: *volume.as_bytes(), operation });
    let result = context.finish(result)?;
    Ok(serde_json::to_value(result)?)
}
fn objects(
    address: playbit::store::volume_index::EntryAddress,
    type_id: Id,
    after: Option<Id>,
    call: &dyn Fn(Request) -> Result<Reply>,
) -> Result<serde_json::Value> {
    let volume = address.volume;
    let entry_id = *address.entry.as_bytes();
    let mut context = DataContext::open(None, call)?;
    let result = (|| {
        let DataResult::Schema(schema) = context.execute(DataCall {
            volume: *volume.as_bytes(),
            operation: DataOperation::SchemaRead { entry_id },
        })?
        else {
            return Err("Unexpected schema result".into());
        };
        let fields: Vec<_> = schema
            .types
            .into_iter()
            .find(|ty| ty.id == *type_id.as_bytes())
            .ok_or("Object type is not in the database schema")?
            .fields
            .into_iter()
            .map(|field| field.id)
            .collect();
        let result = context.execute(DataCall {
            volume: *volume.as_bytes(),
            operation: DataOperation::Query {
                entry_id,
                type_id: *type_id.as_bytes(),
                after: after.map(|id| *id.as_bytes()),
                limit: playbit_api::abi::syscall::MAX_PAGE_OBJECTS,
                projection: (!fields.is_empty()).then_some(fields),
            },
        })?;
        let DataResult::Page(page) = result else {
            return Err("Unexpected query result".into());
        };
        Ok(serde_json::to_value(page)?)
    })();
    context.finish(result)
}

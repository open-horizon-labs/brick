use crate::{Bundle, message};
use anyhow::{Context, Result, ensure};
use prost_reflect::{DynamicMessage, MessageDescriptor, Value};
pub fn encode_bundle(output: MessageDescriptor, bundle: &Bundle) -> Result<DynamicMessage> {
    let mut json = serde_json::to_value(bundle)?;
    json.as_object_mut()
        .context("bundle object")?
        .remove("descriptor");
    let mut response = message(output, &serde_json::json!({"bundle": json}).to_string())?;
    if let Some(Value::Message(value)) = response.get_field_by_name_mut("bundle") {
        value.set_field_by_name("descriptor", Value::Bytes(bundle.descriptor.clone().into()));
    } else {
        anyhow::bail!("missing bundle output");
    }
    Ok(response)
}

pub fn decode_bundle(response: &DynamicMessage) -> Result<Bundle> {
    ensure!(
        response.has_field_by_name("bundle"),
        "generation rejected: {}",
        serde_json::to_string(response)?
    );
    let field = response
        .get_field_by_name("bundle")
        .context("bundle field")?;
    let value = field.as_message().context("bundle message")?;
    let descriptor = value
        .get_field_by_name("descriptor")
        .context("descriptor field")?;
    let descriptor = descriptor.as_bytes().context("descriptor bytes")?;
    let mut json = serde_json::to_value(value)?;
    json["descriptor"] = serde_json::to_value(descriptor.as_ref())?;
    // Protobuf JSON omits empty maps; bundle JSON requires this field.
    if json.get("rustBindings").is_none() {
        json["rustBindings"] = serde_json::json!({});
    }
    let object = json.as_object_mut().context("bundle object")?;
    for (wire, disk) in [
        ("formatVersion", "format_version"),
        ("rustBindings", "rust_bindings"),
    ] {
        if let Some(value) = object.remove(wire) {
            object.insert(disk.into(), value);
        }
    }
    if let Some(compiler) = object.get_mut("compiler").and_then(|v| v.as_object_mut())
        && let Some(value) = compiler.remove("executableDigest")
    {
        compiler.insert("executable_digest".into(), value);
    }
    let bundle: Bundle = serde_json::from_value(json)?;
    bundle.validate()?;
    Ok(bundle)
}

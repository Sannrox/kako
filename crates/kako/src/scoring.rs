//! Batch scoring jobs from model registry descriptors (#54).
//!
//! A scoring job names a signed batch descriptor, the registry keys it
//! trusts, the model artifact, one input dataset and one output. Before
//! anything runs, [`prepare`] checks:
//!
//! - the descriptor signature against a trusted key;
//! - the version manifest it embeds, against a trusted key;
//! - that the manifest names the descriptor's model, version and artifact;
//! - that the metadata digest matches;
//! - the artifact file's SHA-256 against the signed digest.
//!
//! It then generates the Python scoring program from the descriptor's input
//! binding and output fields. The program runs in the sandboxed Python engine
//! with no network, reading the artifact as a read-only staged input, and the
//! runner checks that staged copy's digest again before the run.
//!
//! Output columns follow the descriptor's output fields. A field with one
//! value per row is one column of its name; a field with `n > 1` values per
//! row becomes columns `<name>_0` to `<name>_{n-1}`.

use std::fs;
use std::path::Path;

use base64::Engine as _;
use kako_core::ModelRef;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub const SIGNED_DESCRIPTOR_FORMAT: &str = "hinagata.signed-descriptor/v1";
pub const BATCH_DESCRIPTOR_FORMAT: &str = "hinagata.batch-descriptor/v1";
const SIGNATURE_FORMAT: &str = "hinagata.signature/v1";
const MANIFEST_FORMAT: &str = "hinagata.version-manifest/v1";
/// The staged input name the artifact takes; a scoring job may not use it.
pub const MODEL_INPUT: &str = "model";
/// Tensor dtypes the scoring program can build from input columns.
const DTYPES: &[&str] = &[
    "float16", "float32", "float64", "int8", "int16", "int32", "int64", "uint8", "uint16",
    "uint32", "uint64", "bool",
];

/// A verified scoring job: the program to run and the model it scores with.
#[derive(Debug)]
pub struct Scoring {
    pub code: String,
    pub model: ModelRef,
}

#[derive(Deserialize)]
struct TrustedKeys {
    keys: Vec<TrustedKey>,
}

#[derive(Deserialize)]
struct TrustedKey {
    key_id: String,
    public_key: String,
}

#[derive(Deserialize)]
struct Signed {
    format: String,
    key_id: String,
    descriptor: String,
    signature: String,
}

#[derive(Deserialize)]
struct Envelope {
    format: String,
    key_id: String,
    manifest: String,
    signature: String,
    metadata: String,
}

#[derive(Deserialize)]
struct Manifest {
    format: String,
    namespace: String,
    model: String,
    version: u64,
    artifact_digest: String,
    metadata_digest: String,
}

#[derive(Deserialize)]
struct Descriptor {
    format: String,
    namespace: String,
    model: String,
    version: u64,
    artifact_digest: String,
    version_manifest: Envelope,
    runtime: Runtime,
    signature: Signature,
    input: Source,
    output: Output,
}

#[derive(Deserialize)]
struct Runtime {
    engine: String,
}

#[derive(Deserialize)]
struct Signature {
    inputs: Vec<Tensor>,
}

#[derive(Deserialize)]
struct Tensor {
    name: String,
    dtype: String,
    shape: Vec<i64>,
}

#[derive(Deserialize)]
struct Source {
    binding: Binding,
}

#[derive(Deserialize)]
struct Binding {
    fields: Vec<Field>,
    #[serde(default)]
    inputs: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct Field {
    name: String,
}

#[derive(Deserialize)]
struct Output {
    fields: Vec<OutputField>,
}

#[derive(Deserialize, serde::Serialize)]
struct OutputField {
    name: String,
    dimensions: Vec<i64>,
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// Refuse anything but a regular file, before opening it: a FIFO or device
/// would block loading the job, outside the sandbox and its timeout.
fn regular(path: &Path) -> Result<&Path, String> {
    let meta =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("{} must be a regular file", path.display()));
    }
    Ok(path)
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(regular(path)?).map_err(|error| format!("read {}: {error}", path.display()))
}

fn keys(path: &Path) -> Result<Vec<(String, ed25519_dalek::VerifyingKey)>, String> {
    let file: TrustedKeys = serde_json::from_slice(&read(path)?)
        .map_err(|error| format!("trusted keys {}: {error}", path.display()))?;
    file.keys
        .into_iter()
        .map(|key| {
            let bytes: [u8; 32] = (0..key.public_key.len())
                .step_by(2)
                .map(|at| {
                    key.public_key
                        .get(at..at + 2)
                        .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                })
                .collect::<Option<Vec<u8>>>()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| format!("key `{}` is not 64 hex characters", key.key_id))?;
            let verifying = ed25519_dalek::VerifyingKey::from_bytes(&bytes)
                .map_err(|_| format!("key `{}` is not an Ed25519 key", key.key_id))?;
            Ok((key.key_id, verifying))
        })
        .collect()
}

/// Check `signature` (standard base64) over `bytes` with the trusted key
/// named `key_id`.
fn verify(
    keys: &[(String, ed25519_dalek::VerifyingKey)],
    key_id: &str,
    bytes: &str,
    signature: &str,
    what: &str,
) -> Result<(), String> {
    let key = keys
        .iter()
        .find(|(id, _)| id == key_id)
        .map(|(_, key)| key)
        .ok_or_else(|| format!("{what} is signed by `{key_id}`, which is not trusted"))?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .ok()
        .and_then(|bytes| ed25519_dalek::Signature::from_slice(&bytes).ok())
        .ok_or_else(|| format!("{what} signature is not an Ed25519 signature"))?;
    key.verify_strict(bytes.as_bytes(), &signature)
        .map_err(|_| format!("{what} signature does not verify"))
}

/// Verify the descriptor at `descriptor` against the keys at `trusted_keys`
/// and the artifact at `artifact`, and build the program that scores the
/// input `data` into the output `output`.
pub fn prepare(
    descriptor: &Path,
    trusted_keys: &Path,
    artifact: &Path,
    data: &str,
    output: &str,
) -> Result<Scoring, String> {
    let keys = keys(trusted_keys)?;
    let signed: Signed = serde_json::from_slice(&read(descriptor)?)
        .map_err(|error| format!("descriptor {}: {error}", descriptor.display()))?;
    if signed.format != SIGNED_DESCRIPTOR_FORMAT {
        return Err(format!(
            "descriptor format must be {SIGNED_DESCRIPTOR_FORMAT}"
        ));
    }
    verify(
        &keys,
        &signed.key_id,
        &signed.descriptor,
        &signed.signature,
        "the descriptor",
    )?;
    let described: Descriptor =
        serde_json::from_str(&signed.descriptor).map_err(|error| format!("descriptor: {error}"))?;
    if described.format != BATCH_DESCRIPTOR_FORMAT {
        return Err(format!("a scoring job needs a {BATCH_DESCRIPTOR_FORMAT}"));
    }
    if described.runtime.engine != "onnx" {
        return Err(format!(
            "no scoring runtime for engine `{}`",
            described.runtime.engine
        ));
    }
    let envelope = &described.version_manifest;
    if envelope.format != SIGNATURE_FORMAT {
        return Err(format!(
            "version manifest format must be {SIGNATURE_FORMAT}"
        ));
    }
    verify(
        &keys,
        &envelope.key_id,
        &envelope.manifest,
        &envelope.signature,
        "the version manifest",
    )?;
    let manifest: Manifest = serde_json::from_str(&envelope.manifest)
        .map_err(|error| format!("version manifest: {error}"))?;
    if manifest.format != MANIFEST_FORMAT
        || manifest.namespace != described.namespace
        || manifest.model != described.model
        || manifest.version != described.version
        || manifest.artifact_digest != described.artifact_digest
    {
        return Err("the version manifest names another model version or artifact".into());
    }
    if sha256(envelope.metadata.as_bytes()) != manifest.metadata_digest {
        return Err("the version metadata does not match its signed digest".into());
    }
    // Streamed, so a large model never sits in the runner's memory.
    let actual = kako_core::digest::hash_file(regular(artifact)?)?.0;
    if actual != described.artifact_digest {
        return Err(format!(
            "artifact digest mismatch: the descriptor signs {}, {} is {actual}",
            described.artifact_digest,
            artifact.display()
        ));
    }
    let code = program(&described, data, output)?;
    Ok(Scoring {
        code,
        model: ModelRef {
            namespace: described.namespace,
            model: described.model,
            version: described.version,
            artifact_digest: described.artifact_digest,
            key_id: signed.key_id,
            descriptor_digest: sha256(signed.descriptor.as_bytes()),
        },
    })
}

/// The Python scoring program for `descriptor`.
fn program(descriptor: &Descriptor, data: &str, output: &str) -> Result<String, String> {
    let binding = &descriptor.input.binding;
    let mut inputs = Vec::new();
    for tensor in &descriptor.signature.inputs {
        if !DTYPES.contains(&tensor.dtype.as_str()) {
            return Err(format!(
                "input `{}` has dtype `{}`, which a scoring job cannot build",
                tensor.name, tensor.dtype
            ));
        }
        let rank = tensor.shape.len();
        if !(1..=2).contains(&rank) {
            return Err(format!(
                "input `{}` must be [batch] or [batch, n] to score rows",
                tensor.name
            ));
        }
        let columns = binding
            .inputs
            .get(&tensor.name)
            .cloned()
            .unwrap_or_else(|| vec![tensor.name.clone()]);
        if let Some(missing) = columns
            .iter()
            .find(|column| !binding.fields.iter().any(|field| &field.name == *column))
        {
            return Err(format!(
                "input `{}` reads `{missing}`, which the binding does not list",
                tensor.name
            ));
        }
        // Every bound column must reach the tensor: one for `[batch]`, and
        // exactly `n` for a fixed `[batch, n]`.
        let width = match rank {
            1 => Some(1),
            _ => u64::try_from(tensor.shape[1]).ok().filter(|n| *n > 0),
        };
        if let Some(width) = width
            && columns.len() as u64 != width
        {
            return Err(format!(
                "input `{}` takes {width} columns, the binding gives {}",
                tensor.name,
                columns.len()
            ));
        }
        inputs.push(serde_json::json!({
            "name": tensor.name,
            "columns": columns,
            "dtype": tensor.dtype,
            "rank": rank,
        }));
    }
    let plan = serde_json::json!({
        "data": data,
        "model": MODEL_INPUT,
        "output": output,
        "inputs": inputs,
        "outputs": descriptor.output.fields,
    });
    // A JSON string literal is a valid Python string literal.
    let plan = serde_json::to_string(&plan.to_string()).map_err(|error| error.to_string())?;
    Ok(format!(
        r#""""Batch scoring generated by kako from a verified registry descriptor."""

import json

import numpy as np
import onnxruntime as ort
import polars as pl

PLAN = json.loads({plan})
DTYPES = {{
    "float16": np.float16, "float32": np.float32, "float64": np.float64,
    "int8": np.int8, "int16": np.int16, "int32": np.int32, "int64": np.int64,
    "uint8": np.uint8, "uint16": np.uint16, "uint32": np.uint32, "uint64": np.uint64,
    "bool": np.bool_,
}}


def convert(values, dtype, name):
    """`values` as `dtype`, refusing any value the dtype cannot hold exactly."""
    target = np.dtype(DTYPES[dtype])
    # Decimal, string and other columns would round through float64 first.
    if values.dtype.kind not in "biuf":
        raise ValueError(f"input `{{name}}` reads a column that is not numeric or boolean")
    if target.kind in "iu":
        info = np.iinfo(target)
        if values.dtype.kind in "biu":
            outside = values.size and (values.min() < info.min or values.max() > info.max)
        else:
            values = values.astype(np.float64)
            if not np.all(np.isfinite(values)) or not np.all(np.floor(values) == values):
                raise ValueError(f"input `{{name}}` has values that are not whole numbers")
            # Both bounds are powers of two, so exact as floats: the largest
            # value is compared against `max + 1`, which cannot round down.
            outside = values.size and (
                values.min() < float(info.min) or values.max() >= float(info.max + 1)
            )
        if outside:
            raise ValueError(f"input `{{name}}` has values outside {{dtype}}")
    elif target.kind == "b" and not np.all(np.isin(values, [0, 1])):
        raise ValueError(f"input `{{name}}` has values that are not booleans")
    elif target.kind == "f":
        # Rounding is fine; a finite value turning infinite is not.
        with np.errstate(over="ignore"):
            converted = values.astype(target)
        if np.any(np.isfinite(values.astype(np.float64)) & ~np.isfinite(converted)):
            raise ValueError(f"input `{{name}}` has values outside {{dtype}}")
        return converted
    return values.astype(target)


def transform(inputs):
    data = inputs[PLAN["data"]]
    session = ort.InferenceSession(str(inputs[PLAN["model"]]), providers=["CPUExecutionProvider"])
    feeds = {{}}
    for spec in PLAN["inputs"]:
        # Each column converts on its own, before stacking could promote it.
        columns = [
            convert(data.get_column(column).to_numpy(), spec["dtype"], spec["name"])
            for column in spec["columns"]
        ]
        feeds[spec["name"]] = np.column_stack(columns) if spec["rank"] == 2 else columns[0]
    results = session.run([spec["name"] for spec in PLAN["outputs"]], feeds)
    columns = {{}}

    def put(name, values):
        if name in columns:
            raise ValueError(f"output column `{{name}}` would be written twice")
        columns[name] = values

    for spec, values in zip(PLAN["outputs"], results):
        values = np.asarray(values)
        if values.ndim == 0 or values.shape[0] != len(data):
            raise ValueError(f"output `{{spec['name']}}` does not give one value per row")
        # The tensor's own shape gives the width, so an empty batch flattens too.
        width = int(np.prod(values.shape[1:], dtype=np.int64))
        flat = values.reshape(values.shape[0], width)
        if width == 1:
            put(spec["name"], flat[:, 0])
        else:
            for index in range(width):
                put(f"{{spec['name']}}_{{index}}", flat[:, index])
    return {{PLAN["output"]: pl.DataFrame(columns)}}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scoring")
    }

    fn prepared(artifact: &Path, descriptor: &Path) -> Result<Scoring, String> {
        prepare(
            descriptor,
            &fixtures().join("registry-keys.json"),
            artifact,
            "customers",
            "scores.csv",
        )
    }

    #[test]
    fn a_verified_descriptor_gives_the_model_lineage_and_a_program() {
        let scoring = prepared(
            &fixtures().join("churn.onnx"),
            &fixtures().join("batch-descriptor.json"),
        )
        .unwrap();
        assert_eq!(
            (scoring.model.model.as_str(), scoring.model.version),
            ("churn", 1)
        );
        assert_eq!(
            scoring.model.artifact_digest,
            "sha256:4c5e73927ad2ef3adc6a26be4b776fefb948de9272e985c6eed3979d3a1728fa"
        );
        assert!(scoring.code.contains("onnxruntime"));
        assert!(
            scoring
                .code
                .contains(r#"\"columns\":[\"age\",\"tenure\",\"spend\"]"#)
        );
    }

    #[test]
    fn a_binding_must_feed_each_input_exactly() {
        let descriptor = |inputs: serde_json::Value, shape: serde_json::Value| -> Descriptor {
            serde_json::from_value(serde_json::json!({
                "format": BATCH_DESCRIPTOR_FORMAT, "namespace": "n", "model": "m", "version": 1,
                "artifact_digest": "sha256:00",
                "version_manifest": {"format": "", "key_id": "", "manifest": "", "signature": "", "metadata": ""},
                "runtime": {"engine": "onnx"},
                "signature": {"inputs": [{"name": "x", "dtype": "float32", "shape": shape}]},
                "input": {"binding": {"fields": [{"name": "a"}, {"name": "b"}], "inputs": inputs}},
                "output": {"fields": []}
            }))
            .unwrap()
        };
        let two = serde_json::json!({"x": ["a", "b"]});
        assert!(
            program(
                &descriptor(two.clone(), serde_json::json!([-1, 2])),
                "d",
                "o.csv"
            )
            .is_ok()
        );
        for shape in [serde_json::json!([-1]), serde_json::json!([-1, 3])] {
            let refused = program(&descriptor(two.clone(), shape), "d", "o.csv").unwrap_err();
            assert!(refused.contains("columns"), "{refused}");
        }
        let unknown = serde_json::json!({"x": ["a", "c"]});
        assert!(
            program(
                &descriptor(unknown, serde_json::json!([-1, 2])),
                "d",
                "o.csv"
            )
            .is_err()
        );
    }

    #[test]
    fn a_mismatched_artifact_or_a_tampered_descriptor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.onnx");
        fs::write(&other, b"not the signed model").unwrap();
        let refused = prepared(&other, &fixtures().join("batch-descriptor.json")).unwrap_err();
        assert!(refused.contains("artifact digest mismatch"), "{refused}");
        #[cfg(unix)]
        {
            let link = dir.path().join("link.onnx");
            std::os::unix::fs::symlink(fixtures().join("churn.onnx"), &link).unwrap();
            let refused = prepared(&link, &fixtures().join("batch-descriptor.json")).unwrap_err();
            assert!(refused.contains("regular file"), "{refused}");
        }

        let raw = fs::read_to_string(fixtures().join("batch-descriptor.json")).unwrap();
        let tampered = dir.path().join("tampered.json");
        fs::write(&tampered, raw.replace("churn_scores", "other_scores")).unwrap();
        let refused = prepared(&fixtures().join("churn.onnx"), &tampered).unwrap_err();
        assert!(refused.contains("does not verify"), "{refused}");

        let untrusted = dir.path().join("keys.json");
        fs::write(
            &untrusted,
            r#"{"keys":[{"key_id":"registry-2","public_key":"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"}]}"#,
        )
        .unwrap();
        let refused = prepare(
            &fixtures().join("batch-descriptor.json"),
            &untrusted,
            &fixtures().join("churn.onnx"),
            "customers",
            "scores.csv",
        )
        .unwrap_err();
        assert!(refused.contains("not trusted"), "{refused}");
    }
}

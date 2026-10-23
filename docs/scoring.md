# Batch scoring from a model registry descriptor

A scoring job scores one dataset with a registered model. Kako builds the scoring program itself from a signed batch descriptor (format `hinagata.batch-descriptor/v1`). No user code runs.

## Job file

```json
{
  "id": "score-churn",
  "scoring": {
    "descriptor": "batch-descriptor.json",
    "trusted_keys": "registry-keys.json",
    "artifact": "churn.onnx"
  },
  "inputs": {"customers": "customers.parquet"},
  "outputs": ["scores.parquet"]
}
```

- **`descriptor`:** the registry's signed descriptor response, `hinagata.signed-descriptor/v1`, holding `key_id`, `descriptor` and `signature`.
- **`trusted_keys`:** the registry keys this job trusts, as `{"keys": [{"key_id": "...", "public_key": "<64 hex>"}]}`.
- **`artifact`:** the model file, `.onnx`, for example taken from a registry bundle.
- **`inputs` and `outputs`:** a scoring job reads exactly one input and writes exactly one output.

All paths are relative to the job file. A scoring job runs on the Python engine and sets neither `code` nor `sql`.

## Before anything runs

Kako refuses the job unless every check passes:

1. The descriptor signature verifies with a trusted key.
2. The version manifest embedded in it verifies with a trusted key, names the descriptor's namespace, model, version and artifact digest, and its metadata digest matches.
3. The artifact file's SHA-256 equals the signed artifact digest.
4. Every input tensor is `[batch]` or `[batch, n]` of a numeric or bool dtype, and reads fields the binding lists.

The runner stages the artifact as the input `model` and checks the staged copy against the signed digest again before the sandbox starts. A swapped file is refused even if it changed after the job was loaded.

## The run

The program runs in the sandboxed Python engine with no network, using ONNX Runtime from the hashed lockfile.

- Each input tensor reads the fields its binding lists, or the field of its own name, in order.
- Each column converts to the tensor's dtype on its own. The run fails rather than feed the model a changed value:
  - a column that is not numeric or boolean is refused;
  - a value an integer dtype cannot hold exactly is refused (fractional, out of range, NaN);
  - a value a bool dtype cannot hold is refused;
  - a finite value that overflows a float dtype is refused.

  Ordinary float rounding is allowed.
- Each output field becomes one column of its name.
- An output with `n > 1` values per row becomes the columns `<name>_0` to `<name>_{n-1}`.
- Rows keep the input's order.

## Receipt

The receipt records the run as for any job:
- the digest of the input and of the staged `model`, and the output digest;
- input and output markings, which follow the usual marking rules.

It also gains `model`: the namespace, model, version, artifact digest, the key that signed the descriptor, and the digest of the descriptor bytes.

## Not yet

- Prediction summaries are not sent to registry monitoring. A summary must use the version's drift-baseline bins, and the descriptor does not carry them.
- Partitioning is not split into several runs: one job scores its whole input.

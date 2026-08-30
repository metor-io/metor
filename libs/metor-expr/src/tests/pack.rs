//! The pack backend, held to the harness rule: a compiled pack is only
//! believed once its exports have answered. The full open→bind→execute walk
//! runs under the real `WasmPack` host in `metor-fsw-2`'s suite (this crate
//! cannot depend on its own consumer); here the module's baked answers —
//! ABI word, manifest bytes — are called and read back, the manifest is
//! decoded as the host's own type, and determinism is pinned to the byte,
//! because `entry_identity` and the sidecar hash both ride on it.

use metor_fsw_2_core::abi::{FSW_ABI_VERSION, PackManifest};
use metor_proto::types::{ComponentId, PrimType};
use wasmi::Val;

use crate::{CompSchema, ComponentSource, Dtype, PackResolver, Resolver, Ty, compile_pack};

/// A host with one native producer: instance `imu`, port `sensors`, a
/// three-vector and an `f32` scalar in one record behind an 8-byte
/// timestamp.
struct Imu;

const GYRO: &str = "imu.sensors.gyro_b";
const TEMP: &str = "imu.sensors.temp";

fn source_of(path: &str) -> Option<ComponentSource> {
    let (name, prim, shape, offset): (_, _, &[usize], _) = match path {
        GYRO => ("sensors.gyro_b", PrimType::F64, &[3], 8),
        TEMP => ("sensors.temp", PrimType::F32, &[], 32),
        _ => return None,
    };
    Some(ComponentSource {
        instance: "imu".into(),
        port_name: "sensors".into(),
        frame_id: ComponentId::new("sensors"),
        max_size: 40,
        component_id: ComponentId::new(name),
        component_name: name.into(),
        prim,
        shape: shape.to_vec(),
        offset,
    })
}

impl Resolver for Imu {
    fn component(&self, path: &str) -> Option<CompSchema> {
        let source = source_of(path)?;
        let ty = match source.shape.as_slice() {
            [] => Ty::F64,
            shape => Ty::Tensor {
                dtype: Dtype::F64,
                shape: shape.to_vec(),
            },
        };
        Some(CompSchema { ty })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        [GYRO, TEMP]
            .into_iter()
            .filter(|p| p.ends_with(&format!(".{name}")))
            .map(str::to_string)
            .collect()
    }

    fn frame(&self, _name: &str) -> Option<crate::FrameSchema> {
        None
    }
}

impl PackResolver for Imu {
    fn component_source(&self, path: &str) -> Option<ComponentSource> {
        source_of(path)
    }
}

const NORM: &str = "@system(\"imu.sensors.gyro_b\")\n\
                    def gyro_norm(gyro_b) -> f64:\n    return (gyro_b @ gyro_b) ** 0.5\n";

#[test]
fn a_pack_compile_is_byte_deterministic() {
    let once = compile_pack(NORM, &Imu, 120.0).expect("compiles");
    let twice = compile_pack(NORM, &Imu, 120.0).expect("compiles");
    assert_eq!(once.wasm, twice.wasm, "entry_identity rides on these bytes");
    assert_eq!(once.pack_manifest, twice.pack_manifest);
}

#[test]
fn the_baked_manifest_is_the_hosts_own_type_and_the_module_answers_it() {
    let program = compile_pack(NORM, &Imu, 120.0).expect("compiles");
    assert_eq!(super::imports(&program.wasm), 0, "packs stay closed");

    let manifest: PackManifest =
        postcard::from_bytes(&program.pack_manifest).expect("the host's decode path accepts it");
    let [entry] = manifest.systems.as_slice() else {
        panic!("one @system, one entry");
    };
    assert_eq!(entry.descriptor.name, "gyro_norm");
    assert!(entry.reloadable);
    assert!(entry.params_default.is_none());
    // One grouped input port named for its producer, the Table output, and
    // the log tail every native entry carries.
    assert_eq!(entry.descriptor.inputs.len(), 1);
    assert_eq!(entry.descriptor.inputs[0].name, "sensors");
    assert_eq!(entry.descriptor.inputs[0].max_size, 40);
    let names: Vec<&str> = entry
        .descriptor
        .outputs
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["gyro_norm", "log"]);

    // The module's own answers: the ABI word, and the manifest bytes behind
    // `fsw_pack_describe`/`fsw_pack_manifest_ptr` are the returned ones.
    let (mut store, instance) = super::instantiate(&program.wasm, 100_000_000);
    let mut out = [Val::I32(0)];
    let version = instance.get_func(&store, "fsw_abi_version").unwrap();
    version.call(&mut store, &[], &mut out).unwrap();
    assert_eq!(out[0].i32(), Some(FSW_ABI_VERSION as i32));

    let open = instance.get_func(&store, "fsw_pack_open").unwrap();
    open.call(&mut store, &[], &mut out).unwrap();
    let pack = out[0].clone();
    let mut len = [Val::I64(0)];
    let describe = instance.get_func(&store, "fsw_pack_describe").unwrap();
    describe
        .call(&mut store, std::slice::from_ref(&pack), &mut len)
        .unwrap();
    assert_eq!(len[0].i64(), Some(program.pack_manifest.len() as i64));
    let ptr = instance.get_func(&store, "fsw_pack_manifest_ptr").unwrap();
    ptr.call(&mut store, std::slice::from_ref(&pack), &mut out)
        .unwrap();
    let at = out[0].i32().unwrap() as usize;
    let memory = instance.get_memory(&store, "memory").unwrap();
    let mut bytes = vec![0u8; program.pack_manifest.len()];
    memory.read(&store, at, &mut bytes).unwrap();
    assert_eq!(
        bytes, program.pack_manifest,
        "describe hands back the baked bytes"
    );

    // The expr export family is untouched beside the pack one.
    for export in ["gyro_norm_eval", "gyro_norm_arg_ptr", "expr_manifest_ptr"] {
        assert!(
            instance.get_func(&store, export).is_some(),
            "`{export}` survives a pack compile"
        );
    }
}

#[test]
fn the_pack_gate_rejects_stages_bad_rates_and_the_clockless() {
    let stage = "slow = resample_zoh(imu.sensors.temp, 10.0)\n";
    let diags = compile_pack(stage, &Imu, 120.0).expect_err("stages are panel-only");
    assert!(format!("{diags}").contains("panel-only"), "{diags}");

    let rate = "@system(rate=7.0)\ndef beat() -> f64:\n    return 1.0\n";
    let diags = compile_pack(rate, &Imu, 120.0).expect_err("7 does not divide 120");
    assert!(format!("{diags}").contains("does not divide"), "{diags}");
    compile_pack(
        "@system(rate=60.0)\ndef beat() -> f64:\n    return 1.0\n",
        &Imu,
        120.0,
    )
    .expect("60 divides 120");

    let clockless = "@system\ndef idle() -> f64:\n    return 1.0\n";
    let diags = compile_pack(clockless, &Imu, 120.0).expect_err("nothing fires it");
    assert!(
        format!("{diags}").contains("nothing would ever fire"),
        "{diags}"
    );
}

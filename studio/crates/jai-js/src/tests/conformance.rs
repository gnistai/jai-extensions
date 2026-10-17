//! The spec's conformance suite inside QuickJS (DESIGN.md section 13):
//! `jai/tests/run.mjs`, bundled as `dist/studio/conformance.mjs` with its
//! node: imports shimmed over `globalThis.__host`, runs against
//! `dist/jai.mjs`. Every fixture and the fuzz test must pass.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rquickjs::prelude::Func;
use rquickjs::{Ctx, Exception, Object};
use url::Url;

use super::{plain, run_job, tools_dir};
use crate::runtime::{Entry, Job};

/// The spec's tests directory: `$JAI_SPEC_DIR/tests`, else `../jai/tests`.
fn spec_tests_dir() -> PathBuf {
    let spec = std::env::var_os("JAI_SPEC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| tools_dir().join("..").join("jai"));
    let tests = spec.join("tests");
    assert!(
        tests.is_dir(),
        "{} is missing (set JAI_SPEC_DIR to the jai spec repository)",
        tests.display()
    );
    plain(&tests)
}

/// `path` resolved, if it lies inside `root`.
fn inside(root: &Path, path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    if !candidate.exists() {
        return None;
    }
    let resolved = plain(candidate);
    resolved.starts_with(root).then_some(resolved)
}

/// `__host.readFile`, `readDir` and `exists`, limited to the spec tests directory.
fn install_fs<'js>(ctx: &Ctx<'js>, root: PathBuf) -> rquickjs::Result<()> {
    let host = Object::new(ctx.clone())?;
    let files = root.clone();
    host.set(
        "readFile",
        Func::from(
            move |ctx: Ctx<'js>, path: String| match inside(&files, &path) {
                Some(p) => std::fs::read_to_string(p)
                    .map_err(|e| Exception::throw_message(&ctx, &format!("{path}: {e}"))),
                None => Err(Exception::throw_message(
                    &ctx,
                    &format!("{path} is outside the spec tests"),
                )),
            },
        ),
    )?;
    let dirs = root.clone();
    host.set(
        "readDir",
        Func::from(move |ctx: Ctx<'js>, path: String| {
            let dir = inside(&dirs, &path).ok_or_else(|| {
                Exception::throw_message(&ctx, &format!("{path} is outside the spec tests"))
            })?;
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .map_err(|e| Exception::throw_message(&ctx, &format!("{path}: {e}")))?
                .filter_map(|entry| {
                    entry
                        .ok()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                })
                .collect();
            names.sort();
            Ok::<_, rquickjs::Error>(names)
        }),
    )?;
    host.set(
        "exists",
        Func::from(move |path: String| inside(&root, &path).is_some()),
    )?;
    ctx.globals().set("__host", host)
}

#[test]
fn spec_conformance_suite_passes_inside_quickjs() {
    let tools = tools_dir();
    let bundle = tools.join("dist").join("studio").join("conformance.mjs");
    let library = tools.join("dist").join("jai.mjs");
    let tests = spec_tests_dir();
    let read = |p: &Path| {
        std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("{}: {e} (run npm run build in jai-tools)", p.display()))
    };
    let file_url = |p: &Path| {
        Url::from_file_path(p)
            .map(String::from)
            .unwrap_or_else(|()| panic!("{} is not absolute", p.display()))
    };

    // The runner reads process.argv, exits through process.exit, and finds its
    // fixtures next to import.meta.url (moduleUrl); console.log is captured.
    let entry = format!(
        r#"const lines = [];
        let exit = null;
        Object.assign(globalThis.__host, {{
            argv: ['qjs', 'run.mjs', '--impl', {impl_path}],
            exit: code => {{ exit = code; }},
            cwd: () => {cwd},
            moduleUrl: {runner_url},
        }});
        console.log = (...args) => {{ lines.push(args.map(String).join(' ')); }};
        await import('jai:conformance');
        export default {{ lines, exit }};"#,
        impl_path = serde_json::to_string(&library.to_string_lossy()).unwrap(),
        cwd = serde_json::to_string(&tools.to_string_lossy()).unwrap(),
        runner_url = serde_json::to_string(&file_url(&tests.join("run.mjs"))).unwrap(),
    );
    let mut job = Job::new(Entry::Module {
        name: "conformance-entry.mjs".into(),
        source: entry,
    });
    job.modules = vec![
        ("jai:conformance".into(), read(&bundle)),
        (file_url(&library), read(&library)),
    ];
    let root = tests.clone();
    job.setup = Some(Box::new(move |ctx: &Ctx<'_>| install_fs(ctx, root)));

    let started = Instant::now();
    let result = run_job(job).unwrap_or_else(|e| panic!("the conformance run failed: {e:#}"));
    let lines: Vec<String> = serde_json::from_value(result["lines"].clone()).unwrap_or_default();
    let report = lines.join("\n");
    let lines: Vec<&str> = lines.iter().map(|l| l.trim()).collect();
    eprintln!(
        "{report}\n(conformance inside QuickJS: {:.1} s)",
        started.elapsed().as_secs_f64()
    );

    assert_eq!(result["exit"], 0, "exit code\n{report}");
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("FAIL") || l.starts_with('✗')),
        "failures\n{report}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("all ") && l.ends_with(" fixtures pass")),
        "no summary\n{report}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("✓ fuzz") && l.ends_with(" 0 failures")),
        "no fuzz run\n{report}"
    );
    let classes = lines
        .iter()
        .filter(|l| l.starts_with('✓') && !l.starts_with("✓ fuzz"))
        .count();
    assert!(classes >= 8, "only {classes} fixture classes ran\n{report}");
}

#[test]
fn conformance_host_cannot_read_outside_the_spec_tests() {
    let tests = spec_tests_dir();
    let outside = tools_dir().join("package.json");
    let entry = format!(
        r#"export default () => {{
            const read = p => {{ try {{ return __host.readFile(p).length > 0; }} catch (e) {{ return e.message.includes('outside'); }} }};
            return {{ inside: read({inside}), outside: read({outside}), exists: [__host.exists({inside}), __host.exists({outside}), __host.exists({missing})],
                      dir: __host.readDir({dir}).includes('run.mjs') }};
        }}"#,
        inside = serde_json::to_string(&tests.join("run.mjs").to_string_lossy()).unwrap(),
        outside = serde_json::to_string(&outside.to_string_lossy()).unwrap(),
        missing = serde_json::to_string(&tests.join("nope.json").to_string_lossy()).unwrap(),
        dir = serde_json::to_string(&tests.to_string_lossy()).unwrap(),
    );
    let mut job = Job::new(Entry::Module {
        name: "fs-check.mjs".into(),
        source: entry,
    });
    job.setup = Some(Box::new(move |ctx: &Ctx<'_>| install_fs(ctx, tests)));
    let v = run_job(job).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "inside": true, "outside": true, "exists": [true, false, false], "dir": true })
    );
}

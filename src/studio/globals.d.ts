// Globals that JAI Studio's Rust host (studio/crates/jai-js) installs before it
// runs a bundle. See studio/DESIGN.md section 8.

/** Host functions for dist/studio/host.mjs. */
interface JaiHostFunctions {
  /** imageops::thumbnail: decode, orient, centre-square crop, 16x16 RGBA. */
  thumbnail(bytes: Uint8Array): { width: number; height: number; data: Uint8Array };
  /** One encode event as JSON; Rust parses it and calls the sink. */
  emit(json: string): void;
  /** Writes <render_dir>/<name>; the name must match ^[A-Za-z0-9._-]{1,120}$. */
  saveRender(name: string, bytes: Uint8Array): void;
}

/** Host functions for dist/studio/conformance.mjs (tests only), used by src/studio/shims. */
interface ConformanceHost {
  /** process.argv: [runtime, script, ...args], e.g. ['qjs', 'run.mjs', '--impl', '/abs/dist/jai.mjs']. */
  argv: string[];
  /** process.exit(code); called once at the end of the run. */
  exit(code: number): void;
  /** Base for relative paths in path.resolve. */
  cwd(): string;
  /** A UTF-8 text file. Paths use forward slashes, and may start with a drive letter. */
  readFile(path: string): string;
  /** Entry names in a directory. */
  readDir(path: string): string[];
  exists(path: string): boolean;
  /** import.meta.url of jai/tests/run.mjs; build.mjs bakes in the file URL it bundled from. */
  moduleUrl?: string;
}

declare var __jai: JaiHostFunctions;
/** Set by host-entry while an encode runs; Rust calls it when the cancellation token fires. */
declare var __jaiAbort: (() => void) | undefined;
declare var __host: ConformanceHost;

// bun test scripts/ocr-runtime-pack.test.ts
// The packaging script's runtime handling (scripts/ocr-runtime-pack.ps1): it must refuse a zip that is not the
// pinned official one, and take exactly the allowlisted files out of it. Windows PowerShell 5.1 is enough.
import { afterAll, describe, expect, setDefaultTimeout, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Each case starts Windows PowerShell, which takes seconds on a loaded machine.
setDefaultTimeout(120_000);

const scripts = resolve(import.meta.dir);
const config = resolve(scripts, "..", "config");
const licenses = resolve(scripts, "..", "third_party", "ocr-vlm-licenses");
const lib = join(scripts, "ocr-runtime-pack.ps1");
const work = mkdtempSync(join(tmpdir(), "ocr-runtime-pack-"));
afterAll(() => rmSync(work, { recursive: true, force: true }));

// Windows PowerShell 5.1 started from a PowerShell 7 parent (the CI runner) inherits pwsh's PSModulePath and
// then cannot load its own Microsoft.PowerShell.Utility (Get-FileHash). Let 5.1 rebuild its default path.
const ps51Env = (() => {
  const env = { ...process.env };
  for (const key of Object.keys(env)) if (key.toLowerCase() === "psmodulepath") delete env[key];
  return env;
})();

const sha256 = (path: string) => createHash("sha256").update(readFileSync(path)).digest("hex");

/** Run PowerShell statements after dot-sourcing the library; returns the exit code and output. */
function ps(body: string) {
  const file = join(work, `t-${Math.random().toString(36).slice(2)}.ps1`);
  writeFileSync(file, `$ErrorActionPreference = 'Stop'\n. '${lib}'\n${body}\n`);
  const run = Bun.spawnSync(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", file], { env: ps51Env });
  return { code: run.exitCode, out: run.stdout.toString(), err: run.stderr.toString() };
}

/** A fixture zip holding root files with the given contents. */
function makeZip(name: string, files: Record<string, string>) {
  const dir = join(work, `src-${name}`);
  const zip = join(work, `${name}.zip`);
  const lines = Object.entries(files).map(
    ([file, text]) =>
      `[IO.File]::WriteAllText((Join-Path '${dir}' '${file}'), '${text}')`,
  );
  const run = ps(
    [
      `New-Item -ItemType Directory -Path '${dir}' | Out-Null`,
      ...lines,
      `Add-Type -AssemblyName System.IO.Compression.FileSystem`,
      `[IO.Compression.ZipFile]::CreateFromDirectory('${dir}', '${zip}')`,
    ].join("\n"),
  );
  expect(run.code, run.err).toBe(0);
  return zip;
}

describe("runtime zip checks", () => {
  const zip = makeZip("good", { "llama-server.exe": "server", "ggml.dll": "ggml", "llama-cli.exe": "cli" });
  const good = sha256(zip);
  const other = "0".repeat(64);

  test("a zip equal to the pin and to GitHub's digest is accepted", () => {
    const run = ps(`Assert-OcrRuntimeZip '${zip}' '${good}' '${good.toUpperCase()}'`);
    expect(run.code, run.err).toBe(0);
    // Offline: the pin alone.
    expect(ps(`Assert-OcrRuntimeZip '${zip}' '${good}' ''`).code).toBe(0);
  });

  test("the script refuses a digest mismatch with the pin", () => {
    const run = ps(`Assert-OcrRuntimeZip '${zip}' '${other}' '${good}'`);
    expect(run.code).not.toBe(0);
    expect(run.err).toContain("mismatch");
    expect(run.err).toContain("the pin");
  });

  test("the script refuses a digest mismatch with the digest GitHub reports", () => {
    const run = ps(`Assert-OcrRuntimeZip '${zip}' '${good}' '${other}'`);
    expect(run.code).not.toBe(0);
    expect(run.err).toContain("GitHub reports");
  });

  test("a changed zip no longer matches the pin", () => {
    const tampered = join(work, "tampered.zip");
    const bytes = readFileSync(zip);
    bytes[bytes.length - 1] ^= 0xff;
    writeFileSync(tampered, bytes);
    expect(ps(`Assert-OcrRuntimeZip '${tampered}' '${good}' ''`).code).not.toBe(0);
  });

  test("extraction takes exactly the allowlisted files and nothing else", () => {
    const dest = join(work, "dest");
    const run = ps(`Expand-OcrRuntime '${zip}' @('llama-server.exe','ggml.dll') '${dest}'`);
    expect(run.code, run.err).toBe(0);
    expect(readdirSync(dest).sort()).toEqual(["ggml.dll", "llama-server.exe"]);
    // Byte for byte: the contents are the zip's.
    expect(readFileSync(join(dest, "llama-server.exe"), "utf8")).toBe("server");
  });

  test("an allowlisted file the zip lacks fails the build", () => {
    const run = ps(`Expand-OcrRuntime '${zip}' @('llama-server.exe','ggml-vulkan.dll') '${join(work, "dest2")}'`);
    expect(run.code).not.toBe(0);
    expect(run.err).toContain("ggml-vulkan.dll");
  });
});

describe("the committed pin and allowlist", () => {
  const allow = ps(`(Read-OcrRuntimeAllowlist '${join(config, "ocr-vlm-allowlist.txt")}') -join ','`);
  const names = allow.out.trim().split(",");

  test("the allowlist is the server, its libraries, 14 CPU variants and the OpenMP licence", () => {
    expect(allow.code, allow.err).toBe(0);
    expect(names).toContain("llama-server.exe");
    expect(names).toContain("llama-server-impl.dll");
    expect(names).toContain("ggml-vulkan.dll");
    expect(names).toContain("libomp.dll");
    expect(names).toContain("LICENSE-LLVM-OpenMP");
    expect(names.filter((n) => n.startsWith("ggml-cpu-"))).toHaveLength(14);
    expect(names.length).toBe(new Set(names).size);
    // No other tool exe is shipped.
    expect(names.filter((n) => n.endsWith(".exe"))).toEqual(["llama-server.exe"]);
    expect(names.some((n) => n.startsWith("ggml-rpc"))).toBe(false);
  });

  test("the b11718 pin is the sha256 and size of the official download", () => {
    const run = ps(`$p = Get-OcrRuntimePin 'b11718' '${join(config, "sources.json")}'; "$($p.asset) $($p.sha256) $($p.size)"`);
    expect(run.code, run.err).toBe(0);
    expect(run.out.trim()).toBe(
      "llama-b11718-bin-win-vulkan-x64.zip 237e5736031a9cdcf9eff54e1f4e9bd871945de1d56ec542ecb9ebaf8d45bdb6 32307966",
    );
    expect(ps(`Get-OcrRuntimePin 'b1' '${join(config, "sources.json")}'`).code).not.toBe(0);
  });

  test("allowlist entries are plain file names", () => {
    const bad = join(work, "bad-allowlist.txt");
    writeFileSync(bad, "llama-server.exe\n..\\evil.dll\n");
    expect(ps(`Read-OcrRuntimeAllowlist '${bad}'`).code).not.toBe(0);
    writeFileSync(bad, "# comment\n\nllama-server.exe\nllama-server.exe\n");
    expect(ps(`Read-OcrRuntimeAllowlist '${bad}'`).code).not.toBe(0);
  });

  test("the GitHub digest can be skipped only for a local zip, never for a download", () => {
    const run = Bun.spawnSync([
      "powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", join(scripts, "package.ps1"),
      "-SkipBuild", "-SkipGithubDigest", "-OutDir", join(work, "out"),
    ], { env: ps51Env });
    expect(run.exitCode).not.toBe(0);
    expect(run.stderr.toString()).toContain("-SkipGithubDigest is only for a local -OcrRuntimeZip");
  });

  test("the runtime ships the licences of what is linked into it", () => {
    const dir = licenses;
    // llama.cpp/ggml, nlohmann json, cpp-httplib (statically linked) and BoringSSL (the TLS library).
    expect(readdirSync(dir).sort()).toEqual([
      "LICENSE-boringssl",
      "LICENSE-httplib",
      "LICENSE-jsonhpp",
      "LICENSE-llama.cpp",
    ]);
    for (const name of readdirSync(dir)) expect(readFileSync(join(dir, name), "utf8").length).toBeGreaterThan(500);
  });

  test("the packaging script checks the zip before it extracts anything", () => {
    const script = readFileSync(join(scripts, "package.ps1"), "utf8");
    expect(script.indexOf("Assert-OcrRuntimeZip")).toBeGreaterThan(0);
    expect(script.indexOf("Assert-OcrRuntimeZip")).toBeLessThan(script.indexOf("Expand-OcrRuntime"));
    expect(script).toContain("'ocr-vlm'");
  });

  test("the archive is deterministic: ordinal order, fixed timestamps", () => {
    const script = readFileSync(join(scripts, "package.ps1"), "utf8");
    expect(script).toContain("[StringComparer]::Ordinal");
    expect(script).toContain("[DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)");
    expect(script).toContain("CreateEntry(");
    expect(script).not.toContain("CreateEntryFromFile");
  });

  test("signing is pinned: the exact CLI version, the signing variables cleared", () => {
    const script = readFileSync(join(scripts, "sign.ps1"), "utf8");
    expect(script).toContain("@tauri-apps/cli@2.10.1");
    expect(script).not.toMatch(/bunx\s+tauri\b/);
    for (const name of [
      "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PATH", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
      "TAURI_PRIVATE_KEY", "TAURI_PRIVATE_KEY_PATH", "TAURI_PRIVATE_KEY_PASSWORD",
    ]) expect(script).toContain(`'${name}'`);
  });
});

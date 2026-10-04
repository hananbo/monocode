import { execFileSync, spawn } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { HostStore } from "./store";
import { resolveHostWorktree, resolveHostWorktreeAsync } from "./git-worktrees";

export type HostCow = {
  id: string;
  path: string;
  projectCwd: string;
  sourceCwd: string;
  sessionId: string;
  branch: string;
  head: string;
  missing?: boolean;
  rootIdentity?: [string, string];
};

const filename =
  process.platform === "win32"
    ? "monocode-isolation.exe"
    : "monocode-isolation";
const directory = dirname(fileURLToPath(import.meta.url));
const candidates = [
  join(directory, filename),
  ...(directory === resolve("host")
    ? [resolve("target/release", filename), resolve("target/debug", filename)]
    : []),
];

export function cowHelper(): string | undefined {
  return process.platform === "darwin" ? candidates.find(existsSync) : undefined;
}

function result<T>(output: string): T {
  const value = JSON.parse(output) as {
    ok: boolean;
    result?: T;
    error?: string;
  };
  if (!value.ok)
    throw new Error(value.error || "Copy-on-write operation failed");
  return value.result as T;
}

export function hostCowSync<T>(
  store: HostStore,
  command: string,
  args: Record<string, unknown>,
): T {
  const helper = cowHelper();
  if (!helper)
    throw new Error(
      "Copy-on-write requires APFS and an updated native MonoCode Host on macOS.",
    );
  try {
    return result<T>(
      execFileSync(helper, ["--store", store.isolationDir], {
        input: JSON.stringify({ command, args }),
        encoding: "utf8",
        timeout: 30_000,
        maxBuffer: 16 * 1024 * 1024,
        windowsHide: true,
      }),
    );
  } catch (error) {
    const output = (error as { stdout?: string }).stdout;
    if (output) return result<T>(String(output));
    throw error;
  }
}

export function hostCow<T>(
  store: HostStore,
  command: string,
  args: Record<string, unknown>,
): Promise<T> {
  const helper = cowHelper();
  if (!helper) {
    if (command === "cow_capability")
      return Promise.resolve({
        supported: false,
        reason:
          "Copy-on-write requires APFS and an updated native MonoCode Host on macOS.",
      } as T);
    if (command === "cow_list") return Promise.resolve([] as T);
    return Promise.reject(
      new Error("Copy-on-write requires APFS and an updated native MonoCode Host on macOS."),
    );
  }
  return new Promise((accept, reject) => {
    const child = spawn(helper, ["--store", store.isolationDir], {
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const output: Buffer[] = [];
    const errors: Buffer[] = [];
    let bytes = 0;
    let failure: Error | undefined;
    // A removal caller may restore sessions on failure. Do not report failure
    // while the helper can still mutate the checkout.
    const stop = (error: Error) => {
      failure ??= error;
      child.kill();
    };
    const timer = setTimeout(() => {
      stop(
        new Error(
          "Copy-on-write operation timed out; its recovery record was retained.",
        ),
      );
    }, 10 * 60_000);
    child.stdout.on("data", (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > 16 * 1024 * 1024) {
        stop(new Error("Copy-on-write response is too large."));
      } else output.push(chunk);
    });
    child.stderr.on("data", (chunk: Buffer) => {
      if (errors.length < 64) errors.push(chunk);
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      if (child.pid == null) reject(error); // Spawn failed; no helper exists.
      else failure ??= error; // Failed termination still requires confirmed exit.
    });
    child.stdin.on("error", (error) => {
      stop(error);
    });
    child.on("close", () => {
      clearTimeout(timer);
      if (failure) {
        reject(failure);
        return;
      }
      try {
        accept(result<T>(Buffer.concat(output).toString("utf8")));
      } catch (error) {
        reject(
          output.length
            ? error
            : new Error(
                Buffer.concat(errors).toString("utf8") ||
                  "Copy-on-write helper exited without a response.",
              ),
        );
      }
    });
    child.stdin.end(JSON.stringify({ command, args }));
  });
}

function requestedPath(requested: unknown): string | undefined {
  if (requested == null || requested === "") return undefined;
  if (
    typeof requested !== "string" ||
    requested.includes("\0") ||
    requested.length > 4096
  )
    throw new Error("Invalid working copy");
  return realpathSync.native(requested);
}

export function resolveHostWorkspace(
  store: HostStore,
  projectCwd: string,
  requested: unknown,
): string {
  const actual = requestedPath(requested);
  if (!actual || actual === projectCwd) return projectCwd;
  if (cowHelper()) {
    const copies = hostCowSync<HostCow[]>(store, "cow_list", {
      cwd: projectCwd,
    });
    if (copies.some((copy) => !copy.missing && copy.path === actual))
      return actual;
  }
  return resolveHostWorktree(projectCwd, actual);
}

export async function resolveHostWorkspaceAsync(
  store: HostStore,
  projectCwd: string,
  requested: unknown,
): Promise<string> {
  const actual = requestedPath(requested);
  if (!actual || actual === projectCwd) return projectCwd;
  const copies = await hostCow<HostCow[]>(store, "cow_list", {
    cwd: projectCwd,
  });
  if (copies.some((copy) => !copy.missing && copy.path === actual))
    return actual;
  return resolveHostWorktreeAsync(projectCwd, actual);
}

export function ownedHostCow(
  store: HostStore,
  projectCwd: string,
  id: string,
): HostCow {
  const copy = hostCowSync<HostCow[]>(store, "cow_list", {
    cwd: projectCwd,
  }).find((entry) => entry.id === id && !entry.missing);
  if (!copy)
    throw new Error("This project’s copy-on-write workspace is unavailable.");
  return copy;
}

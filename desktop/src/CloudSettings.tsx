import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

// Mirrors api::CloudStatus.
interface CloudStatus {
  connected: boolean;
  remote_kind: string | null;
  path: string;
  synchronize: boolean;
  rclone_path: string;
  rclone_valid: boolean;
}

export function CloudSettings() {
  const [status, setStatus] = useState<CloudStatus | null>(null);
  const [pathDraft, setPathDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The Google OAuth link, shown so the user can open/paste it manually - rclone's own
  // browser auto-open isn't reliable (e.g. a broken/missing default-browser association
  // just silently does nothing), so this is the primary path, not just a fallback.
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  function refresh() {
    invoke<CloudStatus>("cloud_status")
      .then((s) => {
        setStatus(s);
        setPathDraft(s.path);
      })
      .catch((e) => setError(String(e)));
  }

  useEffect(refresh, []);

  useEffect(() => {
    const unlisten = listen<string>("cloud-auth-url", (event) => {
      setAuthUrl(event.payload);
      setCopied(false);
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  async function connect() {
    setBusy(true);
    setError(null);
    setAuthUrl(null);
    try {
      // Emits "cloud-auth-url" (caught above) as soon as rclone prints the link,
      // well before this resolves - it doesn't return until the user finishes (or
      // abandons) approval in the browser, so it can take a while.
      await invoke("connect_google_drive");
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      setAuthUrl(null);
    }
  }

  async function copyAuthUrl() {
    if (!authUrl) return;
    try {
      await navigator.clipboard.writeText(authUrl);
      setCopied(true);
    } catch {
      // Clipboard API can be denied/unavailable; the field below is still
      // selectable/copyable by hand either way.
    }
  }

  async function disconnect() {
    setBusy(true);
    setError(null);
    try {
      await invoke("disconnect_cloud");
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function savePath() {
    setBusy(true);
    setError(null);
    try {
      await invoke("set_cloud_path", { path: pathDraft });
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function pickRclonePath() {
    const picked = await open({ multiple: false, directory: false });
    if (!picked || typeof picked !== "string") return;
    setBusy(true);
    setError(null);
    try {
      await invoke("set_rclone_path", { path: picked });
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function toggleSynchronize() {
    if (!status) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("set_cloud_synchronize", { enabled: !status.synchronize });
      refresh();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  if (!status) {
    return <section className="cloud-settings">Loading cloud settings...</section>;
  }

  return (
    <section className="cloud-settings">
      <h2>Cloud</h2>

      {error && <p className="error-text">{error}</p>}

      {!status.rclone_valid && (
        <>
          <p className="warning-text">
            rclone not found ({status.rclone_path || "no path set"}). Install it and make
            sure it's on your PATH, or point at the binary directly.
          </p>
          <button disabled={busy} onClick={pickRclonePath}>
            Change rclone path…
          </button>
        </>
      )}

      {status.connected ? (
        <>
          <p>
            Connected: <strong>{status.remote_kind}</strong>
          </p>
          <label className="field">
            Cloud folder
            <input
              value={pathDraft}
              onChange={(e) => setPathDraft(e.currentTarget.value)}
              placeholder="ludusavi-backup"
            />
          </label>
          <div className="row">
            <button disabled={busy || pathDraft === status.path} onClick={savePath}>
              Save folder
            </button>
            <button disabled={busy} onClick={disconnect}>
              Disconnect
            </button>
          </div>
          <label className="checkbox-field">
            <input
              type="checkbox"
              checked={status.synchronize}
              disabled={busy}
              onChange={toggleSynchronize}
            />
            Auto-upload after every backup
          </label>
        </>
      ) : (
        <>
          <p>No cloud remote configured.</p>
          <button disabled={busy || !status.rclone_valid} onClick={connect}>
            {busy ? "Waiting for Google sign-in..." : "Connect Google Drive"}
          </button>
          {authUrl && (
            <div className="auth-url-box">
              <p>
                Open this link in a browser to finish connecting (it may also have opened
                automatically):
              </p>
              <div className="row">
                <input value={authUrl} readOnly onFocus={(e) => e.currentTarget.select()} />
                <button onClick={copyAuthUrl}>{copied ? "Copied!" : "Copy link"}</button>
              </div>
            </div>
          )}
        </>
      )}
    </section>
  );
}

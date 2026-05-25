import type { DoctorReport } from "../api/types";

interface SettingsPageProps {
  doctor: DoctorReport | null;
  loading: boolean;
  error: string | null;
  onClose: () => void;
}

export function SettingsPage({
  doctor,
  loading,
  error,
  onClose,
}: SettingsPageProps) {
  return (
    <div className="approval-overlay">
      <div className="settings-dialog settings-wide">
        <div className="settings-header">
          <h3>Settings</h3>
          <button type="button" onClick={onClose}>
            Close
          </button>
        </div>

        {loading ? <p className="muted">Loading runtime report…</p> : null}
        {error ? <p className="error-inline">{error}</p> : null}

        {doctor ? (
          <div className="settings-grid">
            <section>
              <h4>Provider</h4>
              <dl>
                <dt>API key source</dt>
                <dd>{doctor.api_key.source}</dd>
                <dt>Base URL</dt>
                <dd>{doctor.base_url}</dd>
                <dt>Default model</dt>
                <dd>{doctor.default_model}</dd>
                <dt>Auto model</dt>
                <dd>{doctor.deepseek.auto_model ? "enabled" : "disabled"}</dd>
                <dt>Reasoning effort</dt>
                <dd>{doctor.deepseek.reasoning_effort}</dd>
                <dt>Cost currency</dt>
                <dd>{doctor.deepseek.cost_currency}</dd>
              </dl>
              <p className="hint-text">{doctor.deepseek.api_key_hint}</p>
            </section>

            <section>
              <h4>Sandbox</h4>
              <div className="badge-row">
                <span
                  className={
                    doctor.sandbox.available ? "badge ok" : "badge warn"
                  }
                >
                  {doctor.sandbox.available ? "available" : "unavailable"}
                </span>
                {doctor.sandbox.kind ? (
                  <span className="badge">{doctor.sandbox.kind}</span>
                ) : null}
              </div>
              <p className="hint-text">{doctor.sandbox.detail}</p>
              <p className="hint-text">
                Sandbox is enforced by the agent execution policy when the OS
                supports it. Toggle via env / policy config before launch.
              </p>
            </section>

            <section>
              <h4>MCP</h4>
              <p className="hint-text">{doctor.mcp.config_path}</p>
              {doctor.mcp.servers.length === 0 ? (
                <p className="muted">No MCP servers configured.</p>
              ) : (
                <ul className="settings-list">
                  {doctor.mcp.servers.map((server) => (
                    <li key={server.name}>
                      <strong>{server.name}</strong> · {server.status} ·{" "}
                      {server.tool_count} tools
                      {!server.enabled ? " (disabled)" : ""}
                    </li>
                  ))}
                </ul>
              )}
              {doctor.mcp.errors.map((item) => (
                <p key={item} className="error-inline">
                  {item}
                </p>
              ))}
            </section>

            <section>
              <h4>Skills</h4>
              <p>{doctor.skills.total_count} skill(s) discovered</p>
              <ul className="settings-list">
                {doctor.skills.directories.map((dir) => (
                  <li key={dir.path}>
                    {dir.path} — {dir.count}
                    {!dir.present ? " (missing)" : ""}
                  </li>
                ))}
              </ul>
              {doctor.skills.warnings.map((warning) => (
                <p key={warning} className="hint-text">
                  {warning}
                </p>
              ))}
            </section>

            <section className="settings-full">
              <h4>Models</h4>
              <ul className="settings-list">
                {doctor.deepseek.models.map((model) => (
                  <li key={model.id}>
                    {model.id} · ctx {model.context_window.toLocaleString()} ·
                    tools={model.supports_tools ? "yes" : "no"} · reasoning=
                    {model.supports_reasoning ? "yes" : "no"}
                  </li>
                ))}
              </ul>
            </section>
          </div>
        ) : null}
      </div>
    </div>
  );
}

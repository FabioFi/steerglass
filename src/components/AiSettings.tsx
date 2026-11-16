import { useEffect, useState } from "react";
import { KeyRound, RefreshCw, Trash2 } from "lucide-react";
import { api, errText, type AiKeyStatus, type AiProvider, type AiSettings } from "../api";
import { PROVIDERS, providerOf, saveAiSettings, useAi } from "../ai";
import { useStore } from "../store";
import { SettingGroup, SettingRow } from "./SettingRow";

/** Answer languages: the value goes to the model ("Answer in Italian"); empty is English. */
const LANGUAGES: [string, string][] = [
  ["", "English"],
  ["Italian", "Italiano"],
  ["Spanish", "Español"],
  ["French", "Français"],
  ["German", "Deutsch"],
  ["Portuguese", "Português"],
  ["Dutch", "Nederlands"],
  ["Polish", "Polski"],
  ["Ukrainian", "Українська"],
  ["Turkish", "Türkçe"],
  ["Simplified Chinese", "简体中文"],
  ["Japanese", "日本語"],
  ["Korean", "한국어"],
];

/** Settings → AI: on/off, provider, server, API key (kept in the system keychain) and model. */
export function AiSection() {
  const ai = useAi((s) => s.settings);
  const [key, setKey] = useState("");
  const [keyStatus, setKeyStatus] = useState<AiKeyStatus | null>(null);
  const [models, setModels] = useState<string[] | null>(null);
  const [modelsState, setModelsState] = useState<{ loading: boolean; error?: string }>({ loading: false });
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const { toast } = useStore.getState();

  const provider = ai ? providerOf(ai.provider) : PROVIDERS[0];

  useEffect(() => {
    if (!ai) return;
    setBaseUrl(ai.baseUrl);
    setModel(ai.model);
    setModels(null);
    setModelsState({ loading: false });
    setKey("");
    api.aiKeyStatus(ai.provider).then(setKeyStatus, () => setKeyStatus(null));
    // Only when the provider changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ai?.provider]);

  if (!ai) return null;

  const save = (next: Partial<AiSettings>) => saveAiSettings({ ...ai, ...next }).catch((e) => toast(errText(e), "error"));

  const loadModels = async (settings: AiSettings = ai) => {
    setModelsState({ loading: true });
    try {
      const list = await api.aiModels(settings);
      setModels(list);
      setModelsState({ loading: false });
      if (!settings.model && list.length === 1) {
        setModel(list[0]);
        save({ model: list[0] });
      }
    } catch (e) {
      setModels(null);
      setModelsState({ loading: false, error: errText(e) });
    }
  };

  const saveKey = async (value: string) => {
    try {
      setKeyStatus(await api.aiSetKey(ai.provider, value));
      setKey("");
      if (value) loadModels();
    } catch (e) {
      toast(errText(e), "error");
    }
  };

  const keyDesc =
    provider.key === "none"
      ? "Not needed: the model runs on this computer."
      : keyStatus?.set
        ? keyStatus.storedIn === "file"
          ? "Saved in a file only your user can read (no system keychain was found)."
          : "Saved in the system keychain, never in Steerglass's settings file."
        : provider.key === "optional"
          ? "Only if your server asks for one."
          : provider.hint;

  return (
    <>
      <p className="set-intro">
        Optional. Steerglass can explain why a resource is failing and summarize logs with an AI model you choose. It talks directly to that provider with your
        own key, or to a model running on your computer: nothing goes through Steerglass. Secrets are never sent, and values that look like passwords or tokens
        are hidden. You can always see exactly what is sent.
      </p>
      <SettingGroup title="AI help">
        <SettingRow title="Use AI" desc="Adds Ask AI (⌘J), Explain (key A and next to problems), Summarize on logs, plain words in ⌘K (⇧↵) and Ask for a command in shells.">
          <input type="checkbox" className="switch" checked={ai.enabled} onChange={(e) => save({ enabled: e.target.checked })} aria-label="Use AI" />
        </SettingRow>
      </SettingGroup>
      <SettingGroup title="Model">
        <SettingRow title="Provider" desc={provider.key === "none" || provider.id === "custom" ? provider.hint : undefined}>
          <select
            value={ai.provider}
            onChange={(e) => save({ provider: e.target.value as AiProvider, baseUrl: "", model: "" })}
            aria-label="AI provider"
          >
            {PROVIDERS.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </select>
        </SettingRow>
        <SettingRow title="Server" desc={provider.id === "custom" ? "The address of its OpenAI-compatible API, usually ending in /v1." : "Change it only for a proxy or another region."}>
          <input
            className="text-input set-input mono"
            value={baseUrl}
            placeholder={provider.base || "https://…/v1"}
            spellCheck={false}
            onChange={(e) => setBaseUrl(e.target.value)}
            onBlur={() => baseUrl !== ai.baseUrl && save({ baseUrl: baseUrl.trim() })}
            aria-label="Server address"
          />
        </SettingRow>
        {provider.key !== "none" && (
          <SettingRow title="API key" desc={keyDesc}>
            <div className="row-gap">
              {keyStatus?.set ? (
                <>
                  <span className="muted small">
                    <KeyRound size={12} /> Saved
                  </span>
                  <button className="icon-btn" onClick={() => saveKey("")} aria-label="Remove the API key" title="Remove the API key">
                    <Trash2 size={13} />
                  </button>
                </>
              ) : (
                <>
                  <input
                    className="text-input set-input mono"
                    type="password"
                    value={key}
                    placeholder="Paste your key"
                    spellCheck={false}
                    autoComplete="off"
                    onChange={(e) => setKey(e.target.value)}
                    onKeyDown={(e) => e.key === "Enter" && key.trim() && saveKey(key)}
                    aria-label="API key"
                  />
                  <button className="btn small" disabled={!key.trim()} onClick={() => saveKey(key)}>
                    Save
                  </button>
                </>
              )}
            </div>
          </SettingRow>
        )}
        <SettingRow
          title="Model"
          desc={
            modelsState.error ? (
              <span className="error">{modelsState.error}</span>
            ) : models ? (
              `Connected: ${models.length} model${models.length === 1 ? "" : "s"} available. Larger models explain better; smaller ones answer faster and cost less.`
            ) : (
              "Type a model name, or load the list from the server (this also tests the connection)."
            )
          }
        >
          <div className="row-gap">
            <input
              className="text-input set-input mono"
              list="ai-models"
              value={model}
              placeholder="Model"
              spellCheck={false}
              onChange={(e) => setModel(e.target.value)}
              onBlur={() => model !== ai.model && save({ model: model.trim() })}
              onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
              aria-label="Model"
            />
            <datalist id="ai-models">
              {models?.map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
            <button className="btn small" onClick={() => loadModels({ ...ai, baseUrl: baseUrl.trim(), model })} disabled={modelsState.loading} title="Load the models from the server">
              <RefreshCw size={12} className={modelsState.loading ? "spin" : ""} /> Load
            </button>
          </div>
        </SettingRow>
      </SettingGroup>
      <SettingGroup title="Answers">
        <SettingRow title="Language" desc="The language the AI answers in. Logs, commands and Kubernetes terms stay as they are.">
          <select value={ai.language || ""} onChange={(e) => save({ language: e.target.value })} aria-label="Answer language">
            {LANGUAGES.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </SettingRow>
      </SettingGroup>
      <SettingGroup title="Production">
        <SettingRow
          title="Use on production clusters"
          desc="When off, Explain isn't offered on clusters marked as production, so their data never leaves your computer."
        >
          <input
            type="checkbox"
            className="switch"
            checked={ai.onProduction}
            onChange={(e) => save({ onProduction: e.target.checked })}
            aria-label="Use AI on production clusters"
          />
        </SettingRow>
      </SettingGroup>
    </>
  );
}

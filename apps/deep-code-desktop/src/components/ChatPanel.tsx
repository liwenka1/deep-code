import { type FormEvent, type KeyboardEvent, useEffect, useRef } from "react";
import type { ChatMessage } from "../api/types";

interface ChatPanelProps {
  messages: ChatMessage[];
  streamingText: string;
  isStreaming: boolean;
  status: string;
  error: string | null;
  prompt: string;
  onPromptChange: (value: string) => void;
  onSubmit: () => void;
}

function statusLabel(status?: ChatMessage["toolStatus"]): string | null {
  if (!status) return null;
  return status;
}

export function ChatPanel({
  messages,
  streamingText,
  isStreaming,
  status,
  error,
  prompt,
  onPromptChange,
  onSubmit,
}: ChatPanelProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const scrollKey = `${messages.length}:${streamingText.length}`;

  // biome-ignore lint/correctness/useExhaustiveDependencies: scroll when message content changes
  useEffect(() => {
    const node = scrollRef.current;
    if (node) {
      node.scrollTop = node.scrollHeight;
    }
  }, [scrollKey]);

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault();
    if (!isStreaming && prompt.trim()) {
      onSubmit();
    }
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      if (!isStreaming && prompt.trim()) {
        onSubmit();
      }
    }
  };

  return (
    <section className="chat-panel">
      <div className="messages" ref={scrollRef}>
        {messages.length === 0 && !streamingText ? (
          <div className="empty-state">
            <p>Send a prompt to start chatting with deep-code.</p>
            <p className="hint">
              Tip: in offline mode, try <code>/mock-tool hello</code> to
              exercise tool approval.
            </p>
          </div>
        ) : (
          messages.map((message) => (
            <article
              key={message.id}
              className={`message message-${message.role}${
                message.toolStatus ? ` tool-${message.toolStatus}` : ""
              }`}
            >
              <header>
                {message.role}
                {message.toolName ? ` · ${message.toolName}` : ""}
                {statusLabel(message.toolStatus)
                  ? ` · ${statusLabel(message.toolStatus)}`
                  : ""}
              </header>
              {message.meta ? (
                <div className="message-meta">{message.meta}</div>
              ) : null}
              <div className="message-body">{message.content}</div>
            </article>
          ))
        )}
        {streamingText ? (
          <article className="message message-assistant streaming">
            <header>assistant</header>
            <div className="message-body">{streamingText}</div>
          </article>
        ) : null}
      </div>

      {error ? <div className="error-banner">{error}</div> : null}

      <footer className="composer">
        <div className="status-bar">{status}</div>
        <form onSubmit={handleSubmit}>
          <textarea
            value={prompt}
            onChange={(event) => onPromptChange(event.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Type a prompt… (Enter to send, Shift+Enter for newline)"
            rows={3}
            disabled={isStreaming}
          />
          <button type="submit" disabled={isStreaming || !prompt.trim()}>
            {isStreaming ? "Streaming…" : "Send"}
          </button>
        </form>
      </footer>
    </section>
  );
}

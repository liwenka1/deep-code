import type { SseEnvelope } from "./types";

export async function* readSseStream(
  response: Response,
): AsyncGenerator<SseEnvelope> {
  const reader = response.body?.getReader();
  if (!reader) {
    throw new Error("response has no body");
  }

  const decoder = new TextDecoder();
  let buffer = "";
  let eventType = "";
  let dataLines: string[] = [];

  const flush = (): SseEnvelope | null => {
    if (dataLines.length === 0) {
      return null;
    }
    const raw = dataLines.join("\n");
    dataLines = [];
    const parsed = JSON.parse(raw) as SseEnvelope;
    if (eventType) {
      parsed.event = eventType;
    }
    eventType = "";
    return parsed;
  };

  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    buffer += decoder.decode(value, { stream: true });

    while (true) {
      const lineBreak = buffer.indexOf("\n");
      if (lineBreak === -1) {
        break;
      }
      let line = buffer.slice(0, lineBreak);
      buffer = buffer.slice(lineBreak + 1);
      if (line.endsWith("\r")) {
        line = line.slice(0, -1);
      }

      if (line === "") {
        const event = flush();
        if (event) {
          yield event;
        }
        continue;
      }
      if (line.startsWith(":")) {
        continue;
      }
      if (line.startsWith("event:")) {
        eventType = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        dataLines.push(line.slice(5).trimStart());
      }
    }
  }

  const last = flush();
  if (last) {
    yield last;
  }
}

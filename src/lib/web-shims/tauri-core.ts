export interface InvokeOptions {
  headers?: HeadersInit;
}

export class Channel<T = unknown> {
  onmessage: (message: T) => void = () => {};
}

async function readChannel(
  response: Response,
  channel: Channel,
): Promise<void> {
  if (!response.body) throw new Error("Session stream is unavailable");
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  let complete = false;
  const deliver = (line: string) => {
    if (!line.trim()) return;
    const chunk = JSON.parse(line);
    channel.onmessage(chunk);
    if (chunk.type === "error") throw new Error(chunk.message);
    if (chunk.type === "done") complete = true;
  };
  try {
    while (true) {
      const { value, done } = await reader.read();
      pending += decoder.decode(value, { stream: !done });
      let newline: number;
      while ((newline = pending.indexOf("\n")) !== -1) {
        deliver(pending.slice(0, newline));
        pending = pending.slice(newline + 1);
      }
      if (done) break;
    }
    deliver(pending);
    if (!complete) throw new Error("Session stream ended before completion");
  } finally {
    await reader.cancel();
    reader.releaseLock();
  }
}

export function isTauri(): boolean {
  return false;
}

export async function invoke<T = unknown>(
  command: string,
  args?: Record<string, unknown>,
  options?: InvokeOptions,
): Promise<T> {
  if (command === "export_session_markdown") {
    const url = URL.createObjectURL(
      new Blob([String(args?.content ?? "")], {
        type: "text/markdown;charset=utf-8",
      }),
    );
    const link = document.createElement("a");
    const name = String(args?.defaultName ?? "session.md");
    link.href = url;
    link.download = name;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    return name as T;
  }

  const response = await fetch(`/api/invoke/${encodeURIComponent(command)}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(options?.headers ?? {}),
    },
    body: JSON.stringify({ args: args ?? {} }),
  });

  if (response.ok && command === "get_session_image") {
    return (await response.arrayBuffer()) as T;
  }
  if (response.ok && command === "stream_session_messages") {
    if (!(args?.onChunk instanceof Channel)) {
      throw new Error("Session stream requires a Channel");
    }
    await readChannel(response, args.onChunk);
    return undefined as T;
  }

  const text = await response.text();
  const payload = text ? JSON.parse(text) : null;

  if (!response.ok) {
    const message = payload?.error || `Command failed: ${command}`;
    if (command === "set_proxy_stack_member") {
      let detail: unknown;
      try {
        detail = JSON.parse(message);
      } catch {
        /* plain error message */
      }
      if (
        typeof detail === "object" &&
        detail !== null &&
        "partial" in detail
      ) {
        throw detail;
      }
    }
    throw new Error(message);
  }

  return payload as T;
}

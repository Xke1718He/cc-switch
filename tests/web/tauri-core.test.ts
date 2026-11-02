import { ReadableStream } from "node:stream/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Channel, invoke } from "../../src/lib/web-shims/tauri-core";

function streamResponse(bytes: Uint8Array[]) {
  return {
    ok: true,
    body: new ReadableStream({
      start(controller) {
        bytes.forEach((chunk) => controller.enqueue(chunk));
        controller.close();
      },
    }),
  };
}

afterEach(() => vi.unstubAllGlobals());

describe("web invoke transport", () => {
  it("delivers ordered chunks split across UTF-8 and JSON boundaries", async () => {
    const chunks = [
      { type: "header", total: 1 },
      { type: "messages", messages: [{ content: "你好" }] },
      { type: "done", payloadBytes: 12 },
    ];
    const bytes = new TextEncoder().encode(
      chunks.map((chunk) => JSON.stringify(chunk)).join("\n"),
    );
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          streamResponse(Array.from(bytes, (byte) => Uint8Array.of(byte))),
        ),
    );
    const channel = new Channel();
    const received: unknown[] = [];
    channel.onmessage = (chunk) => received.push(chunk);
    await invoke("stream_session_messages", { onChunk: channel });
    expect(received).toEqual(chunks);
  });

  it("rejects streams which end without a done chunk", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          streamResponse([
            new TextEncoder().encode('{"type":"header","total":0}\n'),
          ]),
        ),
    );
    await expect(
      invoke("stream_session_messages", { onChunk: new Channel() }),
    ).rejects.toThrow("ended before completion");
  });

  it("delivers server stream errors and rejects the invocation", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          streamResponse([
            new TextEncoder().encode(
              '{"type":"error","message":"invalid source"}\n',
            ),
          ]),
        ),
    );
    const channel = new Channel();
    channel.onmessage = vi.fn();
    await expect(
      invoke("stream_session_messages", { onChunk: channel }),
    ).rejects.toThrow("invalid source");
    expect(channel.onmessage).toHaveBeenCalledWith({
      type: "error",
      message: "invalid source",
    });
  });

  it("returns image bytes without parsing JSON", async () => {
    const bytes = Uint8Array.of(137, 80, 78, 71).buffer;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, arrayBuffer: async () => bytes }),
    );
    expect(await invoke("get_session_image")).toBe(bytes);
  });

  it("preserves partial stack write errors for query invalidation", async () => {
    const detail = { partial: true, message: "write failed" };
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        text: async () => JSON.stringify({ error: JSON.stringify(detail) }),
      }),
    );
    await expect(invoke("set_proxy_stack_member")).rejects.toEqual(detail);
  });
});

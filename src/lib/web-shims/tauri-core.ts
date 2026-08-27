export interface InvokeOptions {
  headers?: HeadersInit;
}

export function isTauri(): boolean {
  return false;
}

export async function invoke<T = unknown>(
  command: string,
  args?: Record<string, unknown>,
  options?: InvokeOptions,
): Promise<T> {
  const response = await fetch(`/api/invoke/${encodeURIComponent(command)}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(options?.headers ?? {}),
    },
    body: JSON.stringify({ args: args ?? {} }),
  });

  const text = await response.text();
  const payload = text ? JSON.parse(text) : null;

  if (!response.ok) {
    throw new Error(payload?.error || `Command failed: ${command}`);
  }

  return payload as T;
}

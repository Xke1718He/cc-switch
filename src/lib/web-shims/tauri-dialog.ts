export async function message(message: string, options?: { title?: string }): Promise<void> {
  const title = options?.title ? `${options.title}\n\n` : "";
  window.alert(`${title}${message}`);
}

export async function error(
  message: string,
  options?: { file?: string },
): Promise<void> {
  const source = options?.file ? `[${options.file}]` : "[cc-switch]";
  console.error(source, message);
}

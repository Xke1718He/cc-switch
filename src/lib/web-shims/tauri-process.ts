export async function exit(code = 0): Promise<void> {
  console.warn(`exit(${code}) requested in web mode`);
  window.close();
}

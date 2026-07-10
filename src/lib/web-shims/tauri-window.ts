type UnlistenFn = () => void;

export function getCurrentWindow() {
  return {
    async isMaximized(): Promise<boolean> {
      return false;
    },
    async onResized(_handler: () => void): Promise<UnlistenFn> {
      return () => {};
    },
    async setDecorations(_decorations: boolean): Promise<void> {},
    async minimize(): Promise<void> {},
    async toggleMaximize(): Promise<void> {},
    async close(): Promise<void> {
      window.close();
    },
  };
}

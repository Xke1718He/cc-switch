type UnlistenFn = () => void;

export function getCurrentWindow() {
  return {
    async isMaximized(): Promise<boolean> {
      return false;
    },
    async isFullscreen(): Promise<boolean> {
      return document.fullscreenElement !== null;
    },
    async isResizable(): Promise<boolean> {
      return false;
    },
    async scaleFactor(): Promise<number> {
      return window.devicePixelRatio || 1;
    },
    async onScaleChanged(_handler: () => void): Promise<UnlistenFn> {
      return () => {};
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

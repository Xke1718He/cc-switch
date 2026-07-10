export type UnlistenFn = () => void;

export interface Event<T = unknown> {
  event: string;
  id: number;
  payload: T;
}

export async function listen<T>(
  _event: string,
  _handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  return () => {};
}

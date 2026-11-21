export class Cache {
  private items = new Map<string, string>();

  get(key: string): string | undefined {
    return this.items.get(key);
  }

  set(key: string, value: string): void {
    this.items.set(key, value);
  }
}

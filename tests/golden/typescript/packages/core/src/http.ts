import { Cache } from "./cache";

export class Http {
  constructor(private cache: Cache) {}

  get(url: string): string {
    const hit = this.cache.get(url);
    if (hit !== undefined) {
      return hit;
    }
    const body = this.fetchBody(url);
    this.cache.set(url, body);
    return body;
  }

  private fetchBody(url: string): string {
    return `body of ${url}`;
  }
}

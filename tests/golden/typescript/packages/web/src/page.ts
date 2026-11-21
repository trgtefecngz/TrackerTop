import { Cache, Http, formatDate } from "@golden/core";

export function renderHeader(now: Date): string {
  return `<h1>${formatDate(now)}</h1>`;
}

export function loadPage(url: string): string {
  const http = new Http(new Cache());
  return renderHeader(new Date()) + http.get(url);
}

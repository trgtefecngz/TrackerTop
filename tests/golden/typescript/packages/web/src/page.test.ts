import { renderHeader } from "./page";

test("header shows the date", () => {
  expect(renderHeader(new Date(2020, 0, 2))).toContain("2020-01-02");
});

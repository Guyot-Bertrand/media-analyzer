import { test, it, expect } from "vitest";

test("regex avec apostrophe", () => {
  expect("don't").toMatch(/don't/);
});

test("après la regex", () => {
  covers("HARD.AFTER.01");
});

test("jsx avec apostrophe", () => {
  render(<p>l'image</p>);
});

test("après le jsx", () => {
  covers("HARD.JSX.01");
});

test("identifiant dynamique", () => {
  const n = 1;
  covers(`NAV.${n}.01`);
  covers("HARD.CONCAT." + "01");
});

test.only("seul", () => {
  covers("HARD.ONLY.01");
});

it.skip("ignoré", () => {
  covers("HARD.SKIP.01");
});

test.each([1, 2])("cas %s", (n) => {
  covers("HARD.EACH.01");
});

covers("HARD.OUTSIDE.01");

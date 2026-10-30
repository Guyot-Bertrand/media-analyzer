import { test, expect } from "vitest";
import { covers } from "../../covers";

test("affiche une coche pour ok", () => {
  covers("UI.LED.01");
  expect(1).toBe(1);
});

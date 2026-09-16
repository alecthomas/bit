import { render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App.js";

describe("App", () => {
  beforeEach(() => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify([{ id: 1, name: "ada", created_at: "" }]), {
        headers: { "Content-Type": "application/json" },
      }),
    );
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("renders fetched users", async () => {
    render(<App />);
    expect(await screen.findByText("ada")).toBeInTheDocument();
  });
});

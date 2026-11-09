import { render, screen } from "@testing-library/react";
import { describe, it, vi } from "vitest";
import App from "./App";

vi.mock("./api/tauri", () => ({
  listConnections: vi.fn(async () => []),
}));

describe("App", () => {
  it("renders without crashing", async () => {
    render(<App />);
    await screen.findByText("Saved connections");
  });
});

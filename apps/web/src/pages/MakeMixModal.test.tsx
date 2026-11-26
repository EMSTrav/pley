import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { playlistUrl } from "@/lib/urls";
import { makeItem } from "@/test/fixtures";
import { LocationDisplay, mockApi, renderWithProviders } from "@/test/render";
import { MakeMixModal } from "./MakeMixModal";

function open(ai: boolean) {
  const onClose = vi.fn();
  const requests = mockApi({
    "GET /api/mixes": () => ({ ai, max_artists: 2, max_length: 100 }),
    "GET /api/libraries/5/items": () => ({
      total: 3,
      items: ["Another", "Band", "Third"].map((title, i) => makeItem({ id: 11 + i, kind: "artist", title })),
    }),
    "POST /api/mixes": () => ({ playlist_id: 77, title: "Band Mix", item_count: 9, by_ai: ai }),
  });
  renderWithProviders(
    <>
      <Routes>
        <Route path="/" element={<MakeMixModal libraryId={5} onClose={onClose} />} />
      </Routes>
      <LocationDisplay />
    </>,
    { route: "/" },
  );
  return { onClose, requests };
}

describe("MakeMixModal", () => {
  it("makes a mix of the artists picked, up to the most, and opens it", async () => {
    const user = userEvent.setup();
    const { onClose, requests } = open(false);
    const make = screen.getByRole("button", { name: "Make the mix" });
    expect(make).toBeDisabled();
    expect(await screen.findByText("Pick up to 2 artists")).toBeInTheDocument();

    await user.click(await screen.findByRole("checkbox", { name: "Band" }));
    await user.click(screen.getByRole("checkbox", { name: "Third" }));
    expect(screen.getByRole("checkbox", { name: "Another" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Remove Third" }));
    expect(screen.getByRole("checkbox", { name: "Another" })).toBeEnabled();

    await user.type(screen.getByRole("textbox", { name: "Find an artist" }), "ano");
    expect(screen.queryByRole("checkbox", { name: "Band" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: "Another" }));
    expect(screen.queryByLabelText("What's it for?")).not.toBeInTheDocument();
    await user.selectOptions(screen.getByLabelText("Length"), "25");
    await user.click(make);

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent(playlistUrl(77)));
    expect(requests.find((r) => r.method === "POST")?.body).toEqual({ artist_ids: [12, 11], length: 25 });
    expect(onClose).toHaveBeenCalled();
  });

  it("sends what the mix is for when an AI service makes it", async () => {
    const user = userEvent.setup();
    const { requests } = open(true);
    await user.click(await screen.findByRole("checkbox", { name: "Band" }));
    await user.type(await screen.findByLabelText("What's it for?"), "  a long drive ");
    await user.click(screen.getByRole("button", { name: "Make the mix" }));
    await waitFor(() =>
      expect(requests.find((r) => r.method === "POST")?.body).toEqual({ artist_ids: [12], length: 50, prompt: "a long drive" }),
    );
  });
});

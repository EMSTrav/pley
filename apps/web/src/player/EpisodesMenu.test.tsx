import { useState } from "react";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ItemSummary } from "@/api/types";
import { makeItem } from "@/test/fixtures";
import { mockApi, renderWithProviders } from "@/test/render";
import type { QueuePosition } from "@/lib/videoQueue";
import { EpisodesMenu } from "./EpisodesMenu";

const state = (position_ms = 0, watched = false) => ({ position_ms, watched, play_count: watched ? 1 : 0 });

const episode = (id: number, title: string, season: number, index: number, extra: Partial<ItemSummary> = {}) =>
  makeItem({
    id,
    kind: "episode",
    title,
    parent_id: 10 + season,
    grandparent_id: 10,
    grandparent_title: "The Show",
    parent_index: season,
    index,
    user_state: state(),
    ...extra,
  });

function Menu({ item, queue = null, onPick }: { item: ItemSummary; queue?: QueuePosition | null; onPick: (i: ItemSummary, n: number | null) => void }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <p>outside</p>
      <EpisodesMenu item={item} queue={queue} open={open} setOpen={setOpen} onPick={onPick} />
    </div>
  );
}

describe("EpisodesMenu", () => {
  it("lists the season playing, switches seasons, and plays the one picked", async () => {
    const user = userEvent.setup();
    const pilot = episode(1, "Pilot", 1, 1, { user_state: state(0, true) });
    const second = episode(2, "Second", 1, 2);
    const later = episode(3, "Later", 2, 1, { user_state: state(60_000) });
    mockApi({
      "GET /api/items/10/children": () => ({
        items: [makeItem({ id: 11, kind: "season", title: "Season 1", index: 1 }), makeItem({ id: 12, kind: "season", title: "Season 2", index: 2 })],
      }),
      "GET /api/items/11/children": () => ({ items: [pilot, second] }),
      "GET /api/items/12/children": () => ({ items: [later] }),
    });
    const onPick = vi.fn();
    renderWithProviders(<Menu item={second} onPick={onPick} />);

    await user.click(screen.getByRole("button", { name: "Episodes" }));
    const list = await screen.findByRole("dialog", { name: "Episodes" });
    // What plays is marked; what was watched says so.
    expect(await within(list).findByRole("button", { name: /Second/ })).toHaveAttribute("aria-current", "true");
    expect(within(list).getByRole("button", { name: /Pilot/ })).toHaveTextContent("Watched");

    await user.selectOptions(await within(list).findByRole("combobox", { name: "Season" }), "12");
    await user.click(await within(list).findByRole("button", { name: /Later/ }));
    expect(onPick).toHaveBeenCalledWith(later, null);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("lists a queue in its order, dims what it played, and plays an entry by its place", async () => {
    const user = userEvent.setup();
    const a = episode(5, "Five", 3, 5);
    const b = episode(2, "Two", 1, 2);
    const c = episode(9, "Nine", 4, 1);
    const requests = mockApi({ "GET /api/items": () => ({ items: [a, b, c] }) });
    const onPick = vi.fn();
    const queue = { queue: { key: "k", ids: [5, 2, 7, 9] }, index: 1 };
    renderWithProviders(<Menu item={b} queue={queue} onPick={onPick} />);

    await user.click(screen.getByRole("button", { name: "Queue" }));
    const list = await screen.findByRole("dialog", { name: "Queue" });
    expect(requests.at(-1)?.path).toBe("/api/items?ids=5,2,7,9");
    expect(within(list).getByText("2 of 4")).toBeInTheDocument();
    // An entry the profile can't see (7) is left out; the rest stay in the queue's order.
    const titles = (await within(list).findAllByRole("button")).map((b) => b.textContent);
    expect(titles.map((t) => t?.match(/Five|Two|Nine/)?.[0])).toEqual(["Five", "Two", "Nine"]);
    expect(within(list).getByRole("button", { name: /Five/ })).toHaveClass("text-white/45");
    expect(within(list).getByRole("button", { name: /Two/ })).toHaveAttribute("aria-current", "true");

    // Picking what plays only closes the list.
    await user.click(within(list).getByRole("button", { name: /Two/ }));
    expect(onPick).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Queue" }));
    await user.click(await screen.findByRole("button", { name: /Nine/ }));
    expect(onPick).toHaveBeenCalledWith(c, 3);
  });

  it("closes on Escape and on a click elsewhere", async () => {
    const user = userEvent.setup();
    mockApi({ "GET /api/items": () => ({ items: [] }) });
    const item = episode(1, "Pilot", 1, 1);
    renderWithProviders(<Menu item={item} queue={{ queue: { key: "k", ids: [1] }, index: 0 }} onPick={vi.fn()} />);

    await user.click(screen.getByRole("button", { name: "Queue" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Queue" }));
    await user.click(screen.getByText("outside"));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

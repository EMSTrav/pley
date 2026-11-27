import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { LONG_DELAY_MS, useTooltip } from "./Tooltip";

/** The tip on screen, if any: hidden from screen readers, which have the names already. */
const tip = () => screen.queryByRole("tooltip", { hidden: true });

function Icon({ name, label, delayMs }: { name: string | undefined; label: string; delayMs?: number }) {
  const { trigger, tip } = useTooltip(name, { delayMs });
  return (
    <>
      <button {...trigger} type="button" aria-label={label}>
        ·
      </button>
      {tip}
    </>
  );
}

function Icons({ names }: { names: (string | undefined)[] }) {
  return (
    <>
      {names.map((name, i) => (
        <Icon key={i} name={name} label={name ?? `Icon ${i}`} />
      ))}
    </>
  );
}

describe("Tooltip", () => {
  it("shows an icon's name a moment after the pointer comes, the next icon's at once", async () => {
    const user = userEvent.setup();
    render(<Icons names={["Home", "Movies"]} />);
    await user.hover(screen.getByRole("button", { name: "Home" }));
    expect(tip()).toBeNull();
    await waitFor(() => expect(tip()).toHaveTextContent("Home"));

    await user.hover(screen.getByRole("button", { name: "Movies" }));
    expect(screen.getByText("Movies", { selector: '[role="tooltip"]' })).toBeInTheDocument();
    await user.unhover(screen.getByRole("button", { name: "Movies" }));
    await waitFor(() => expect(tip()).toBeNull());
  });

  it("waits longer where asked, even right after another tip", async () => {
    const user = userEvent.setup();
    render(
      <>
        <Icon name="Settings" label="Settings" />
        <Icon name="Expand" label="Expand" delayMs={LONG_DELAY_MS} />
      </>,
    );
    await user.hover(screen.getByRole("button", { name: "Settings" }));
    await waitFor(() => expect(tip()).toHaveTextContent("Settings"));
    await user.hover(screen.getByRole("button", { name: "Expand" }));
    await new Promise((r) => setTimeout(r, 600));
    expect(tip()).toBeNull();
    await waitFor(() => expect(tip()).toHaveTextContent("Expand"), { timeout: 1500 });
  });

  it("shows at once on keyboard focus, and Escape closes it", async () => {
    const user = userEvent.setup();
    render(<Icons names={["Home"]} />);
    await user.tab();
    expect(tip()).toHaveTextContent("Home");
    await user.keyboard("{Escape}");
    await waitFor(() => expect(tip()).toBeNull());
  });

  it("closes when its icon is pressed, and shows nothing for an icon with no name", async () => {
    const user = userEvent.setup();
    render(<Icons names={["Home", undefined]} />);
    await user.hover(screen.getByRole("button", { name: "Home" }));
    await waitFor(() => expect(tip()).toHaveTextContent("Home"));
    await user.pointer({ keys: "[MouseLeft>]", target: screen.getByRole("button", { name: "Home" }) });
    await waitFor(() => expect(tip()).toBeNull());
    await user.pointer({ keys: "[/MouseLeft]" });

    await user.hover(screen.getByRole("button", { name: "Icon 1" }));
    await new Promise((r) => setTimeout(r, 500));
    expect(tip()).toBeNull();
  });
});

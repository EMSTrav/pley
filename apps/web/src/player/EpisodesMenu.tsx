import { useEffect, useMemo, useRef, useState } from "react";
import { useQueries } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { ListVideo } from "lucide-react";
import { api } from "@/api/client";
import { useChildren } from "@/api/hooks";
import type { Id, ItemSummary } from "@/api/types";
import { Artwork } from "@/components/Artwork";
import { cn } from "@/lib/cn";
import { episodeLabel, progressFraction, seasonTitle } from "@/lib/format";
import { thumbCandidates } from "@/lib/images";
import type { QueuePosition } from "@/lib/videoQueue";

/** The most items `GET /api/items?ids=` gives at once. */
const PAGE = 200;

/** One entry of the list: what it is, and its place in the queue (null for a season's). */
interface Entry {
  item: ItemSummary;
  index: number | null;
}

/** A queue's entries, a page of ids at a time, in its order; those the profile can't see are left out. */
function useQueueEntries(queue: QueuePosition | null, enabled: boolean): { entries: Entry[]; loading: boolean } {
  const queueIds = queue?.queue.ids;
  const ids = useMemo(() => queueIds ?? [], [queueIds]);
  const pages = useMemo(() => {
    const out: Id[][] = [];
    for (let i = 0; i < ids.length; i += PAGE) out.push(ids.slice(i, i + PAGE));
    return out;
  }, [ids]);
  const results = useQueries({
    queries: pages.map((page) => ({
      queryKey: ["items", "many", page.join(",")],
      queryFn: ({ signal }: { signal: AbortSignal }) => api.get<{ items: ItemSummary[] }>(`/api/items?ids=${page.join(",")}`, { signal }),
      enabled,
      staleTime: 30_000,
    })),
  });
  const entries = useMemo(() => {
    const byId = new Map<Id, ItemSummary>();
    for (const r of results) for (const item of r.data?.items ?? []) byId.set(item.id, item);
    return ids.flatMap((id, index) => {
      const item = byId.get(id);
      return item ? [{ item, index }] : [];
    });
  }, [ids, results]);
  return { entries, loading: results.some((r) => r.isPending) };
}

/**
 * The player's list of what it plays, opened from the control bar: a
 * queue's entries (a shuffled show, a playlist, a collection), or for an
 * episode played on its own its season's episodes, with the show's other
 * seasons a pick away. What plays is marked and what a queue has played is
 * dimmed; picking another plays it, where it was left.
 */
export function EpisodesMenu({
  item,
  queue,
  open,
  setOpen,
  onPick,
}: {
  item: ItemSummary;
  queue: QueuePosition | null;
  open: boolean;
  setOpen: (open: boolean) => void;
  /** `index`: its place in the queue, null for a season's episode. */
  onPick: (picked: ItemSummary, index: number | null) => void;
}) {
  const { t } = useTranslation("player");
  const ref = useRef<HTMLDivElement>(null);
  const activeRef = useRef<HTMLButtonElement>(null);
  const [season, setSeason] = useState<Id | null>(item.parent_id ?? null);
  const showId = queue ? null : (item.grandparent_id ?? null);
  const seasons = useChildren(showId, open).data?.filter((s) => s.kind === "season") ?? [];
  const episodes = useChildren(queue ? null : season, open);
  const fromQueue = useQueueEntries(queue, open);
  const entries: Entry[] = queue ? fromQueue.entries : (episodes.data ?? []).map((ep) => ({ item: ep, index: null }));
  const loading = queue ? fromQueue.loading : episodes.isPending;
  const isCurrent = (e: Entry) => (queue ? e.index === queue.index : e.item.id === item.id);

  // Each time it opens, scrolled to what plays (on the season playing: see its button).
  useEffect(() => {
    if (open) activeRef.current?.scrollIntoView({ block: "center" });
  }, [open, loading]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", onDown);
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      document.removeEventListener("keydown", onKey, true);
    };
  }, [open, setOpen]);

  const label = queue ? t("episodes.queue") : t("episodes.title");
  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        aria-label={label}
        title={label}
        aria-expanded={open}
        onClick={() => {
          // It opens on the season playing.
          if (!open) setSeason(item.parent_id ?? null);
          setOpen(!open);
        }}
        className={cn("flex size-10 items-center justify-center rounded-full transition hover:bg-white/15", open && "bg-white/15")}
      >
        <ListVideo className="size-5" />
      </button>
      {open ? (
        <div
          role="dialog"
          aria-label={label}
          className="absolute bottom-full right-0 mb-2 flex max-h-[min(70vh,34rem)] w-[min(26rem,calc(100vw-2rem))] animate-pop-in flex-col rounded-lg border border-white/10 bg-black/90 shadow-2xl"
        >
          <div className="flex items-center justify-between gap-3 px-4 pb-2 pt-3">
            <span className="text-sm font-semibold text-white">{label}</span>
            {queue ? (
              <span className="text-xs tabular-nums text-white/60">
                {t("episodes.position", { n: queue.index + 1, count: queue.queue.ids.length })}
              </span>
            ) : seasons.length > 1 ? (
              <select
                aria-label={t("episodes.season")}
                value={season ?? ""}
                onChange={(e) => setSeason(Number(e.target.value))}
                className="h-8 rounded-md border border-white/15 bg-white/10 pl-2 pr-8 text-sm text-white focus:border-accent focus:outline-none"
              >
                {seasons.map((s) => (
                  <option key={s.id} value={s.id} className="bg-neutral-900">
                    {seasonTitle(s)}
                  </option>
                ))}
              </select>
            ) : null}
          </div>
          <ul className="min-h-0 flex-1 overflow-y-auto overscroll-contain px-1.5 pb-1.5">
            {loading && entries.length === 0 ? (
              <li className="px-2.5 py-6 text-center text-sm text-white/60">{t("episodes.loading")}</li>
            ) : null}
            {entries.map((entry) => {
              const current = isCurrent(entry);
              const played = queue != null && entry.index != null && entry.index < queue.index;
              const ep = entry.item;
              const progress = ep.user_state.watched ? 0 : progressFraction(ep);
              const where = ep.kind === "episode" ? episodeLabel(ep) : ep.year ? String(ep.year) : "";
              return (
                <li key={entry.index ?? ep.id}>
                  <button
                    ref={current ? activeRef : undefined}
                    type="button"
                    aria-current={current ? "true" : undefined}
                    onClick={() => {
                      setOpen(false);
                      if (!current) onPick(ep, entry.index);
                    }}
                    className={cn(
                      "flex w-full items-center gap-3 rounded-md px-2.5 py-2 text-left text-sm transition-colors hover:bg-white/10",
                      current ? "bg-white/10 text-accent" : played ? "text-white/45" : "text-white/90",
                    )}
                  >
                    <span className={cn("relative w-28 shrink-0 overflow-hidden rounded", played && "opacity-50")}>
                      <Artwork candidates={thumbCandidates(ep)} width={112} alt="" title={ep.title} className="aspect-video w-full" />
                      {progress > 0 ? (
                        <span className="absolute inset-x-0 bottom-0 h-1 bg-black/60">
                          <span className="block h-full bg-accent" style={{ width: `${progress * 100}%` }} />
                        </span>
                      ) : null}
                    </span>
                    <span className="min-w-0 flex-1">
                      {queue && ep.grandparent_title ? (
                        <span className="block truncate text-xs text-white/60">{ep.grandparent_title}</span>
                      ) : null}
                      <span className="line-clamp-2 font-medium leading-snug">{ep.title}</span>
                      <span className="block text-xs text-white/50">
                        {current ? t("episodes.playing") : ep.user_state.watched ? `${where} · ${t("episodes.watched")}` : where}
                      </span>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

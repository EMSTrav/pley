import { useMemo, useState, type FormEvent } from "react";
import { useNavigate } from "react-router";
import { useTranslation } from "react-i18next";
import { Check, Search, Sparkles, X } from "lucide-react";
import { errorMessage } from "@/api/client";
import { useLibraryItems } from "@/api/hooks";
import { useMakeMix, useMixInfo } from "@/api/lists";
import type { Id } from "@/api/types";
import { Modal } from "@/components/Modal";
import { useToast } from "@/components/Toast";
import { Button, Field, Select, Spinner, TextInput } from "@/components/ui";
import { cn } from "@/lib/cn";
import { playlistUrl } from "@/lib/urls";

const LENGTHS = [25, 50, 100];
/** Artists listed at once; the search narrows the rest. */
const SHOWN = 60;

/** Make a mix from a few of a music library's artists; it's kept as a playlist. */
export function MakeMixModal({ onClose, libraryId }: { onClose: () => void; libraryId: Id }) {
  const { t } = useTranslation(["pages", "common"]);
  const navigate = useNavigate();
  const toast = useToast();
  const info = useMixInfo();
  const artists = useLibraryItems(libraryId, { sort: "title", order: "asc" });
  const make = useMakeMix();
  const [chosen, setChosen] = useState<Id[]>([]);
  const [search, setSearch] = useState("");
  const [length, setLength] = useState(50);
  const [prompt, setPrompt] = useState("");
  const most = info.data?.max_artists ?? 5;
  const ai = info.data?.ai ?? false;

  const all = useMemo(() => artists.data?.items ?? [], [artists.data]);
  const names = useMemo(() => new Map(all.map((a) => [a.id, a.title])), [all]);
  const shown = useMemo(() => {
    const words = search.trim().toLocaleLowerCase();
    return all.filter((a) => !words || a.title.toLocaleLowerCase().includes(words)).slice(0, SHOWN);
  }, [all, search]);
  const toggle = (id: Id) =>
    setChosen((c) => (c.includes(id) ? c.filter((x) => x !== id) : c.length < most ? [...c, id] : c));

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!chosen.length) return;
    make.mutate(
      { artist_ids: chosen, length, ...(ai && prompt.trim() ? { prompt: prompt.trim() } : {}) },
      {
        onSuccess: (mix) => {
          toast.success(t("makeMix.made", { title: mix.title, count: mix.item_count }));
          onClose();
          void navigate(playlistUrl(mix.playlist_id));
        },
        onError: (err) => toast.error(errorMessage(err)),
      },
    );
  };

  return (
    <Modal
      open
      onClose={onClose}
      title={t("makeMix.title")}
      locked={make.isPending}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={make.isPending}>
            {t("common:actions.cancel")}
          </Button>
          <Button
            variant="primary"
            type="submit"
            form="make-mix"
            icon={<Sparkles className="size-4" />}
            loading={make.isPending}
            disabled={!chosen.length}
          >
            {make.isPending ? t("makeMix.making") : t("makeMix.make")}
          </Button>
        </>
      }
    >
      <form id="make-mix" onSubmit={onSubmit} className="space-y-4">
        <p className="text-sm text-muted">{ai ? t("makeMix.describeAi") : t("makeMix.describe")}</p>
        <div className="flex min-h-8 flex-wrap gap-2" aria-live="polite">
          {chosen.length ? (
            chosen.map((id) => (
              <button
                key={id}
                type="button"
                onClick={() => toggle(id)}
                className="inline-flex items-center gap-1 rounded-full bg-accent/15 px-3 py-1 text-sm text-fg hover:bg-accent/25"
                aria-label={t("makeMix.remove", { artist: names.get(id) ?? "" })}
              >
                {names.get(id)}
                <X className="size-3.5" />
              </button>
            ))
          ) : (
            <span className="py-1 text-sm text-muted">{t("makeMix.pick", { count: most })}</span>
          )}
        </div>
        <div className="relative">
          <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted" />
          <TextInput
            aria-label={t("makeMix.search")}
            placeholder={t("makeMix.search")}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-full pl-9"
          />
        </div>
        <div className="max-h-64 overflow-y-auto rounded-md border border-border" role="group" aria-label={t("makeMix.artists")}>
          {artists.isPending ? (
            <div className="flex justify-center p-6">
              <Spinner />
            </div>
          ) : shown.length === 0 ? (
            <p className="p-4 text-sm text-muted">{t("makeMix.noArtists")}</p>
          ) : (
            shown.map((artist) => {
              const on = chosen.includes(artist.id);
              const full = !on && chosen.length >= most;
              return (
                <button
                  key={artist.id}
                  type="button"
                  role="checkbox"
                  aria-checked={on}
                  disabled={full}
                  onClick={() => toggle(artist.id)}
                  className={cn(
                    "flex w-full items-center justify-between px-3 py-2 text-left text-sm transition-colors",
                    on ? "bg-accent/10 text-fg" : "text-fg hover:bg-surface-2",
                    full && "cursor-not-allowed opacity-50",
                  )}
                >
                  <span className="truncate">{artist.title}</span>
                  {on ? <Check className="size-4 shrink-0 text-accent" /> : null}
                </button>
              );
            })
          )}
        </div>
        {ai ? (
          <Field label={t("makeMix.prompt")} htmlFor="make-mix-prompt" hint={t("makeMix.promptHint")}>
            <TextInput
              id="make-mix-prompt"
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              placeholder={t("makeMix.promptExample")}
              maxLength={200}
            />
          </Field>
        ) : null}
        <Field label={t("makeMix.length")} htmlFor="make-mix-length">
          <Select id="make-mix-length" value={length} onChange={(e) => setLength(Number(e.target.value))}>
            {LENGTHS.map((n) => (
              <option key={n} value={n}>
                {t("makeMix.tracks", { count: n })}
              </option>
            ))}
          </Select>
        </Field>
      </form>
    </Modal>
  );
}

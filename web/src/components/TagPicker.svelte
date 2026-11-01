<script lang="ts">
    import { cn } from "$lib/utils.js";

    let {
        name,
        type,
        initial = "",
        placeholder = "",
        inputClass,
        id,
    }: {
        name: string;
        type: string;
        initial?: string;
        placeholder?: string;
        inputClass?: string;
        id?: string;
    } = $props();

    const split = (value: string): string[] =>
        value
            .split(",")
            .map((token) => token.trim())
            .filter((token) => token !== "");

    let mounted = $state(false);
    let chips = $state(split(initial));
    let value = $state("");
    let open = $state(false);
    let suggestions = $state<string[]>([]);
    let activeIndex = $state(0);
    let searching = $state(false);
    let inputEl = $state<HTMLInputElement>(null!);

    $effect(() => {
        mounted = true;
        if (inputEl) inputEl.value = "";
    });

    const term = (): string => (mounted ? value.trim() : (split(initial).at(-1) ?? ""));

    $effect(() => {
        const t = term();
        const timer: ReturnType<typeof setTimeout> = setTimeout(async () => {
            if (t.length < 2) {
                suggestions = [];
                open = false;
                searching = false;
                return;
            }
            searching = true;
            open = true; // popup opens immediately with the spinner, then fills in
            try {
                const response = await fetch(`/api/autocomplete?type=${type}&term=${encodeURIComponent(t)}`);
                const names: string[] = await response.json();
                suggestions = names;
                activeIndex = 0;
                open = names.length > 0 && term() === t;
            } catch {
                suggestions = [];
                open = false;
            } finally {
                searching = false;
            }
        }, 200);
        return () => clearTimeout(timer);
    });

    const select = (tag: string) => {
        if (!chips.includes(tag)) chips = [...chips, tag];
        value = "";
        if (inputEl) inputEl.value = "";
        open = false;
        inputEl?.focus();
    };

    const removeChip = (tag: string) => {
        chips = chips.filter((chip) => chip !== tag);
    };

    const onKeydown = (event: KeyboardEvent) => {
        if (!open || suggestions.length === 0) return;
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            event.preventDefault();
            activeIndex =
                (activeIndex + (event.key === "ArrowDown" ? 1 : -1) + suggestions.length) % suggestions.length;
        } else if (event.key === "Enter") {
            event.preventDefault();
            select(suggestions[activeIndex]);
        } else if (event.key === "Escape") {
            open = false;
        }
    };
</script>

<div class="relative">
    {#if mounted}
        <input type="hidden" {name} value={chips.join(", ")} />
    {/if}
    {#if chips.length > 0}
        <ul class="mb-1 flex flex-wrap gap-1 list-disc list-inside">
            {#each chips as chip (chip)}
                <li>
                    {chip}
                    <button
                        type="button"
                        class="hover:text-destructive cursor-pointer"
                        aria-label={`Remove ${chip}`}
                        onclick={() => removeChip(chip)}
                    >
                        &times;
                    </button>
                </li>
            {/each}
        </ul>
    {/if}

    <input
        bind:this={inputEl}
        {id}
        name={mounted ? undefined : name}
        value={mounted ? value : initial}
        {placeholder}
        type="text"
        autocomplete="off"
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={open}
        class={cn(
            "flex h-9 w-full rounded-md border border-slate-300 bg-white px-2 py-1 text-sm outline-none focus:border-slate-500",
            inputClass,
        )}
        onkeydown={onKeydown}
        oninput={(event) => (value = event.currentTarget.value)}
        onblur={() => (open = false)}
    />

    {#if open}
        <div
            class="bg-popover text-popover-foreground ring-foreground/10 absolute top-full left-0 z-50 mt-1 max-h-72 w-full overflow-auto rounded-md p-1 text-sm shadow-md ring-1"
            role="listbox"
        >
            {#if suggestions.length === 0 && !searching}
                <div class="text-muted-foreground px-2 py-1.5">No tags found.</div>
            {:else}
                {#each suggestions as suggestion, index (suggestion)}
                    <div role="option" aria-selected={index === activeIndex}>
                        <button
                            type="button"
                            class={cn(
                                "flex w-full cursor-default items-center rounded-sm px-2 py-1.5 text-left select-none hover:bg-accent hover:text-accent-foreground",
                                index === activeIndex && "bg-accent text-accent-foreground",
                            )}
                            onmousedown={(event) => event.preventDefault()}
                            onclick={() => select(suggestion)}
                        >
                            {suggestion}
                        </button>
                    </div>
                {/each}
            {/if}
            {#if searching}
                <div
                    class="absolute inset-0 flex items-center justify-center rounded-md bg-background/10 backdrop-blur-[1px]"
                >
                    <span
                        class="border-muted-foreground border-t-foreground size-4 animate-spin rounded-full border-2"
                        aria-label="Loading"
                    ></span>
                </div>
            {/if}
        </div>
    {/if}
</div>

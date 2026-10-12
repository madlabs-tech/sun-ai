<!-- Support from people: the plan's answer target, a form to ask Sun, and the
     conversations so far. Free and Community point to the community instead. -->
<script lang="ts">
  import { Badge, Button, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { Account, Ticket } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { clock, day } from '../lib/format';

  const PLAN_NAME = { community: 'Community', free: 'Free', pro: 'Pro', business: 'Business' };
  const MAX_MESSAGE = 5000;

  let account = $state<Account | null>(null);
  let unavailable = $state(false);
  let tickets = $state<Ticket[]>([]);
  let form = $state({ subject: '', body: '' });
  let errors = $state<{ subject?: string; body?: string }>({});
  let sending = $state(false);

  api.get_account().then(
    async (a) => {
      account = a;
      if (a.support.target_hours) tickets = await api.list_tickets().catch(() => []);
    },
    () => (unavailable = true),
  );

  const when = (at: string) => `${day(at)} ${clock(at)}`;

  async function send(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      subject: form.subject.trim() ? undefined : 'Say in a few words what it is about.',
      body: form.body.trim() ? undefined : 'Describe what happened and what you expected.',
    };
    if (errors.subject || errors.body) return;
    sending = true;
    try {
      const ticket = await api.open_ticket(form.subject.trim(), form.body.trim());
      tickets = [ticket, ...tickets];
      form = { subject: '', body: '' };
      toast('success', 'Sent to Sun', ticket.respond_by ? `We answer by ${when(ticket.respond_by)}.` : undefined);
    } catch (err) {
      toast('danger', 'Not sent', error_text(err));
    }
    sending = false;
  }
</script>

<section class="sun-row" aria-labelledby="support-head">
  <span class="sun-tile"><Icon name="life-buoy" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="support-head">
      Support
      {#if account?.support.target_hours}<Badge tone="info">Answer within {account.support.target_hours} h</Badge>{/if}
    </div>
    {#if unavailable || (account && !account.support.target_hours)}
      <p class="sun-row__meta support__copy">
        Support from people comes with the Pro and Business plans (answers within 48 and 24 hours). Until then, the community
        forum and the docs answer most questions.
      </p>
    {:else if account}
      <p class="sun-row__meta support__copy">
        {PLAN_NAME[account.plan]} plan: a person at Sun answers within {account.support.target_hours} hours. Never paste secrets here.
      </p>
      <form class="support__form" aria-label="Ask Sun" onsubmit={send} novalidate>
        <Input label="Subject" maxlength={200} placeholder="e.g. Codex steps stop after an hour" bind:value={form.subject} error={errors.subject} />
        <label class="ml-field">
          <span class="ml-field__label">Message</span>
          <span class="ml-control support__area" class:ml-control--error={errors.body}>
            <textarea rows="5" maxlength={MAX_MESSAGE} bind:value={form.body} aria-invalid={errors.body ? 'true' : undefined}
              placeholder="What happened, which task or step, and what you expected."></textarea>
          </span>
          {#if errors.body}<span class="ml-field__hint ml-field__hint--error" role="alert">{errors.body}</span>{:else}<span class="ml-field__hint">Task keys and step names help us find it fast.</span>{/if}
        </label>
        <div><Button type="submit" variant="ice" loading={sending}>Send to Sun</Button></div>
      </form>

      {#if tickets.length}
        <ul class="tickets" aria-label="Your questions">
          {#each tickets as ticket (ticket.id)}
            <li class="tickets__item">
              <div class="tickets__head">
                <span class="tickets__subject">{ticket.subject}</span>
                <Badge tone={ticket.status === 'answered' ? 'success' : 'warning'} dot>
                  {ticket.status === 'answered' ? 'Answered' : 'Waiting for Sun'}
                </Badge>
              </div>
              {#if ticket.status === 'open' && ticket.respond_by}
                <span class="tickets__meta">Answer due by {when(ticket.respond_by)}</span>
              {/if}
              <ol class="thread">
                {#each ticket.messages as message (message.created_at + message.author)}
                  <li class="thread__msg" class:thread__msg--sun={message.author === 'sun'}>
                    <span class="tickets__meta">{message.author === 'sun' ? 'Sun' : 'You'} · {when(message.created_at)}</span>
                    <p>{message.body}</p>
                  </li>
                {/each}
              </ol>
            </li>
          {/each}
        </ul>
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</section>

<style>
  .support__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .support__form {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-4);
    max-width: 480px;
  }
  .support__area {
    height: auto;
    padding: var(--space-2) var(--space-3);
  }
  .support__area textarea {
    width: 100%;
    border: 0;
    outline: 0;
    resize: vertical;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: var(--size-body);
    line-height: 1.5;
  }
  .tickets {
    display: grid;
    gap: var(--space-4);
    margin: var(--space-5) 0 0;
    padding: 0;
    list-style: none;
    max-width: 560px;
  }
  .tickets__item {
    display: grid;
    gap: var(--space-2);
  }
  .tickets__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .tickets__subject {
    font-size: 14px;
    font-weight: 600;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .tickets__meta {
    font-size: 12px;
    color: var(--fg-3);
  }
  .thread {
    display: grid;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .thread__msg {
    padding: var(--space-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
  }
  .thread__msg--sun {
    border-color: var(--border-default);
    background: var(--surface-sunken);
  }
  .thread__msg p {
    margin: var(--space-1) 0 0;
    font-size: 14px;
    line-height: 1.5;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
</style>

<!-- The ecosystem's 2-step rules, for `manage_settings`: require a second step at sign-in (people without one
     add one before anything else), and ask for it to change a password. Each switch saves at once. -->
<script lang="ts">
  import { Icon, Switch } from '@hryer/madlabs-design-system/svelte';
  import { auth, type SecurityPolicy } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';

  let policy = $state<SecurityPolicy | null>(null);
  let load_error = $state('');
  auth.get_security().then(
    (p) => (policy = p),
    (e) => (load_error = error_text(e)),
  );

  async function save(change: Partial<SecurityPolicy>) {
    if (!policy) return;
    const before = policy;
    policy = { ...policy, ...change };
    try {
      policy = await auth.set_security(policy);
      toast('success', '2-step rules saved');
    } catch (err) {
      policy = before;
      toast('danger', '2-step rules not saved', error_text(err));
    }
  }
</script>

<section class="sun-row" aria-labelledby="two-step-policy-head">
  <span class="sun-tile"><Icon name="shield-check" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="two-step-policy-head">2-step rules</div>
    <p class="sun-row__meta policy__copy">
      For everyone who signs in with a password. Single sign-on skips these: the identity provider handles 2-step.
    </p>
    {#if policy}
      <div class="policy__switches">
        <Switch
          label="Require 2-step verification"
          checked={policy.require_two_step}
          onchange={(e: Event) => save({ require_two_step: (e.currentTarget as HTMLInputElement).checked })}
        />
        <p class="sun-row__meta policy__hint">People without a second step add one at their next sign-in.</p>
        <Switch
          label="Ask for it to change a password"
          checked={policy.two_step_for_password_change}
          onchange={(e: Event) => save({ two_step_for_password_change: (e.currentTarget as HTMLInputElement).checked })}
        />
      </div>
    {:else if load_error}
      <p class="sun-row__meta" role="alert">{load_error}</p>
    {/if}
  </div>
</section>

<style>
  .policy__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .policy__switches {
    display: grid;
    gap: var(--space-2);
    margin-top: var(--space-4);
  }
  .policy__hint {
    margin: 0 0 var(--space-2);
  }
</style>

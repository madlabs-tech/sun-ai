<!-- Where Sun Cloud's hosted checkout sends the browser back: #/billing/return?status=success|canceled&workspace=<id>.
     Says what happened and links back to Plan & billing. The plan itself changes when the cloud confirms the payment. -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { href } from '../lib/router.svelte';

  const status = new URLSearchParams(location.hash.split('?')[1] ?? '').get('status');
  const RESULT = {
    success: { icon: 'circle-check', title: 'Payment received', text: 'Sun Cloud confirms it in a moment: the plan and seats update within 5 minutes.' },
    canceled: { icon: 'circle-x', title: 'Checkout canceled', text: 'Nothing was charged. Start again from Plan & billing when you’re ready.' },
    unknown: { icon: 'info', title: 'Back from checkout', text: 'Plan & billing shows whether the payment went through.' },
  };
  const result = RESULT[status === 'success' || status === 'canceled' ? status : 'unknown'];
</script>

<PageHeader title="Billing" />

<section class="sun-page" aria-labelledby="return-head">
  <div class="sun-row">
    <span class="sun-tile"><Icon name={result.icon} size={18} /></span>
    <div class="sun-row__body" role="status">
      <h2 class="sun-row__title" id="return-head">{result.title}</h2>
      <p class="sun-row__meta back__copy">{result.text}</p>
    </div>
  </div>
  <div><Button href={href('settings', 'plan')} variant="ice" iconLeft="arrow-left">Back to Plan & billing</Button></div>
</section>

<style>
  .back__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
</style>

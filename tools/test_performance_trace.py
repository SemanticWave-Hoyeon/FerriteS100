"""Pure statistics/ledger tests. No native chart viewer is started."""
import unittest
from pathlib import Path
from unittest.mock import patch, MagicMock
from performance_trace import summarize_ns, summarize_ms, summarize_rows


def rows():
    return [dict(frame=i, hidden=True, focused=False,
        trajectory='chart_relative' if i < 400 else 'outside_wide',
        chart_aabb_overlap_fraction=1 if i < 400 else 0,
        prepare_wall_ns=i, render_through_present_wall_ns=i,
        handler_through_scheduling_wall_ns=i,
        redraw_entry_interval_ns=i if i else None) for i in range(500)]


class StatisticsTests(unittest.TestCase):
    def test_nearest_rank_and_missing_population(self):
        s = summarize_ns(list(range(1,101)) + [None])
        self.assertEqual(s['p50_ns'],50)
        self.assertEqual(s['p95_ns'],95)
        self.assertEqual(s['p99_ns'],99)
        self.assertEqual(s['available_samples'],100)
        self.assertEqual(s['expected_samples'],101)
        self.assertEqual(s['unavailable_samples'],1)
        self.assertIsNone(summarize_ns([None])['over_budget']['60']['fraction'])
        self.assertIsNone(summarize_ns([])['p99_ns'])

    def test_rational_budget_boundaries_and_explicit_denominator(self):
        s = summarize_ns([16_666_666,16_666_667,None])
        self.assertEqual(s['over_budget']['60'],dict(count=1,denominator=2,fraction=.5))
        s = summarize_ns([6_944_444,6_944_445])
        self.assertEqual(s['over_budget']['144']['count'],1)
        for invalid in (True, -1, 1.0, float('nan')):
            with self.assertRaises(RuntimeError): summarize_ns([invalid])

    def test_interaction_population_and_cross_window_intervals(self):
        r = rows()
        r[100]['redraw_entry_interval_ns'] = 1_000_000_000
        s = summarize_rows(r)
        self.assertEqual(s['warm_chart300']['interaction_callbacks'],300)
        interval=s['warm_chart300']['metrics']['within_window_redraw_entry_interval_ns']
        self.assertEqual(interval['available_samples'],299)
        self.assertLess(interval['p99_ns'],400)
        self.assertEqual(s['first_chart100']['metrics']['prepare_wall_ns']['available_samples'],100)
        self.assertEqual(s['outside100']['interaction_callbacks'],100)

    def test_ledger_rejects_incomplete_reordered_focused_or_offchart_primary(self):
        with self.assertRaises(RuntimeError): summarize_rows(rows()[:-1])
        for field, value in [('frame',4),('focused',True),('hidden',False),('chart_aabb_overlap_fraction',0)]:
            r=rows();r[0][field]=value
            with self.assertRaises(RuntimeError): summarize_rows(r)

    def test_gpu_population_is_separate_and_missing_is_not_zero(self):
        s=summarize_ms([.68,None,.0])
        self.assertEqual(s['available_samples'],2)
        self.assertEqual(s['unavailable_samples'],1)
        self.assertEqual(s['mean_ms'],.34)
        self.assertNotIn('over_budget',s)
        self.assertIsNone(summarize_ms([None])['mean_ms'])
        for invalid in (True,-1,float('inf'),float('nan')):
            with self.assertRaises(RuntimeError): summarize_ms([invalid])

    def test_child_is_reaped_even_when_process_receipt_write_fails(self):
        from performance_trace import own_child
        process = MagicMock()
        process.poll.return_value = None
        reader = MagicMock()
        with patch('performance_trace.subprocess.Popen', return_value=process), \
             patch('performance_trace.threading.Thread', return_value=reader), \
             patch('performance_trace.dump', side_effect=OSError('receipt write failed')):
            with self.assertRaises(OSError):
                own_child(['unused'], Path('/private/tmp'), {}, Path('/private/tmp/not-created'))
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=10)
        process.stdout.close.assert_called_once()
        reader.join.assert_called_once_with(timeout=5)


if __name__=='__main__': unittest.main()

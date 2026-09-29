package dev.aas.android.ui

import android.app.Application
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Modifier
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.aas.android.appContainer
import dev.aas.android.domain.ErrorTexts
import dev.aas.android.domain.ForkChoices
import dev.aas.android.domain.PlanChoices
import dev.aas.android.domain.ResumeFailedChoices
import dev.aas.android.domain.composer.SendConfirmation
import dev.aas.android.protocol.Item
import dev.aas.android.protocol.ItemStatus
import dev.aas.android.protocol.NoticeLevel
import dev.aas.android.protocol.StatusRow
import dev.aas.android.protocol.StatusSection
import dev.aas.android.protocol.ThreadHarnessStatusResult
import dev.aas.android.protocol.ThreadModes
import dev.aas.android.protocol.ThreadSettings
import dev.aas.android.protocol.TurnError
import dev.aas.android.protocol.TurnStatus
import dev.aas.android.protocol.UserMessageDelivery
import dev.aas.android.testing.Fixtures
import dev.aas.android.testing.TestEngine
import dev.aas.android.testing.item
import dev.aas.android.ui.common.LocalAppContainer
import dev.aas.android.ui.composer.ModelSheet
import dev.aas.android.ui.theme.AasTheme
import dev.aas.android.ui.thread.ComposerInsertOffer
import dev.aas.android.ui.thread.HarnessStatusState
import dev.aas.android.ui.thread.ItemActions
import dev.aas.android.ui.thread.ItemView
import dev.aas.android.ui.thread.PlanAheadDialog
import dev.aas.android.ui.thread.ProjectTrustBanner
import dev.aas.android.ui.thread.SideAnswer
import dev.aas.android.ui.thread.SideQuestionSheet
import dev.aas.android.ui.thread.SideQuestionState
import dev.aas.android.ui.thread.StatusSheet
import dev.aas.android.ui.thread.TurnEndRow
import dev.aas.android.ui.thread.TurnStartRow
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.annotation.Config
import kotlin.test.assertEquals

/**
 * The harnesses' own features as they are drawn (Robolectric + Compose, docs/android.md 31): a
 * proposed plan with its ways to implement it, 裏に回す, a returned steer, the daemon's notices,
 * a turn's fork menu, a failed resume's lead-in and actions, the `/btw` sheet, the status
 * sheet's modes and harness section, the offer of composer text and the trust banner.
 */
@RunWith(AndroidJUnit4::class)
@Config(application = TestAasApplication::class, qualifiers = "w411dp-h891dp-xxhdpi")
class FeatureRenderingTest {
    @get:Rule
    val compose = createComposeRule()

    private val read = Fixtures.threadRead

    private fun content(scroll: Boolean = true, body: @Composable () -> Unit) {
        val container = ApplicationProvider.getApplicationContext<Application>().appContainer
        compose.setContent {
            CompositionLocalProvider(LocalAppContainer provides container) {
                AasTheme(dynamicColor = false) {
                    if (scroll) Column(Modifier.verticalScroll(rememberScrollState())) { body() } else body()
                }
            }
        }
    }

    @Test
    fun aProposedPlanOffersToImplementItTheHarnesssWays() {
        val plan = read.item<Item.ProposedPlan>()
        val implemented = mutableListOf<String>()
        content {
            ItemView(
                plan,
                ItemActions.None.copy(onImplementPlan = { implemented += "same" }, onImplementPlanInNewThread = { implemented += "new" }),
                planChoices = PlanChoices(implement = true, newThread = true),
            )
        }
        compose.onNodeWithText("提案されたプラン").assertIsDisplayed()
        compose.onNodeWithText("実装する").performClick()
        compose.onNodeWithText("新しいスレッドで実装").performClick()
        assertEquals(listOf("same", "new"), implemented)
    }

    @Test
    fun aProposedPlanWithoutChoicesIsOnlyThePlan() {
        content { ItemView(read.item<Item.ProposedPlan>(), ItemActions.None) }
        compose.onNodeWithText("提案されたプラン").assertIsDisplayed()
        assertEquals(0, compose.onAllNodes(hasText("実装する")).fetchSemanticsNodes().size)
    }

    @Test
    fun aRunningCommandTheHarnessCanMoveOffersToMoveItToTheBackground() {
        val running = read.items.filterIsInstance<Item.CommandExecution>().single { it.backgroundable }
        val moved = mutableListOf<Item>()
        content {
            ItemView(running, ItemActions.None.copy(onMoveToBackground = { moved += it }), moveToBackground = false)
            ItemView(running.copy(id = "queued"), ItemActions.None, moveToBackground = true)
        }
        compose.onNodeWithText("裏に回す").performClick()
        assertEquals(listOf<Item>(running), moved)
        compose.onNodeWithText("裏に回す要求の送信待ち…").assertIsDisplayed()
    }

    @Test
    fun aReturnedSteerAndTheDaemonsNoticesSayWhatHappened() {
        val message = read.item<Item.UserMessage>().copy(status = ItemStatus.Declined, delivery = UserMessageDelivery.Steer)
        val notice = read.item<Item.Notice>().copy(level = NoticeLevel.Info, message = "the agent moved to session b", code = "nativeSessionChanged")
        content {
            ItemView(message, ItemActions.None)
            ItemView(notice, ItemActions.None)
        }
        compose.onNodeWithText("ハーネスが取り込まなかったため、キューに戻しました").assertIsDisplayed()
        compose.onNodeWithText("エージェントが別のセッションに移りました").assertIsDisplayed()
        compose.onNodeWithText("the agent moved to session b").assertIsDisplayed()
    }

    @Test
    fun aTurnsMenuForksAtItOrEditsItsPrompt() {
        val turn = read.turns.first()
        val chosen = mutableListOf<String>()
        content { TurnStartRow(turn, ForkChoices(here = true, editPrompt = true), onForkHere = { chosen += "here" }, onEditPrompt = { chosen += "edit" }) }
        compose.onNodeWithContentDescription("ターン 1 の操作").performClick()
        compose.onNodeWithText("ここから分岐").performClick()
        compose.onNodeWithContentDescription("ターン 1 の操作").performClick()
        compose.onNodeWithText("このプロンプトを編集").performClick()
        assertEquals(listOf("here", "edit"), chosen)
    }

    @Test
    fun aTurnWithoutForkChoicesHasNoMenu() {
        content { TurnStartRow(read.turns.first()) }
        assertEquals(0, compose.onAllNodes(hasContentDescription("ターン 1 の操作")).fetchSemanticsNodes().size)
    }

    @Test
    fun aFailedResumeSaysSoInJapaneseThenTheHarnessesWordsAndOffersItsWaysOut() {
        val turn = read.turns.first().copy(status = TurnStatus.Failed, error = TurnError("session 7c2d is held by another process", ErrorTexts.RESUME_FAILED))
        val chosen = mutableListOf<String>()
        content { TurnEndRow(turn, onOpenDiff = {}, resumeFailed = ResumeFailedChoices(retry = true, fork = true), onRetry = { chosen += "retry" }, onForkNew = { chosen += "fork" }) }
        compose.onNodeWithText("PC のセッションを再開できませんでした\nsession 7c2d is held by another process").assertIsDisplayed()
        compose.onNodeWithText("再試行").performClick()
        compose.onNodeWithText("新しいスレッドに分岐").performClick()
        assertEquals(listOf("retry", "fork"), chosen)
    }

    @Test
    fun theSideQuestionSheetShowsTheAnswerOutsideTheConversation() {
        content(scroll = false) { SideQuestionSheet(SideQuestionState("where is the timer?", SideAnswer.Answered("In `conn.rs`.", synthetic = true)), onDismiss = {}) }
        compose.onNodeWithText("会話とは別の質問").assertIsDisplayed()
        compose.onNodeWithText("where is the timer?").assertIsDisplayed()
        compose.onNodeWithText("モデルの答えではなく、ハーネスが返した定型の答えです").assertIsDisplayed()
        compose.onNodeWithText("この質問と答えは会話の履歴に残りません").assertIsDisplayed()
    }

    @Test
    fun theStatusSheetShowsTheModesTheTrustAndTheHarnessesOwnStatus() {
        val harness = TestEngine.fakeHarness().copy(
            features = dev.aas.android.protocol.HarnessFeatures(status = true, projectTrust = true, fastModeModels = listOf("small")),
        )
        val thread = read.thread.copy(modes = ThreadModes(plan = true, fast = true), fastModeState = "cooldown")
        val status = HarnessStatusState.Loaded(ThreadHarnessStatusResult(listOf(StatusSection("Usage limits", listOf(StatusRow("5-hour", "13% used")))), live = true))
        val trust = mutableListOf<Boolean>()
        content(scroll = false) { StatusSheet(thread, harness, null, onDismiss = {}, harnessStatus = status, trust = null, onSetTrust = { trust += it }) }
        compose.onNodeWithText("プランモード\n高速モード（ハーネスの報告: cooldown）").assertIsDisplayed()
        compose.onNodeWithText("まだ決めていません").performScrollTo()
        compose.onNodeWithText("信頼する").performClick()
        assertEquals(listOf(true), trust)
        compose.onNodeWithText("13% used").performScrollTo()
        compose.onNodeWithText("13% used").assertExists()
        compose.onNodeWithText("エージェントのセッションから").assertExists()
    }

    /**
     * `/plan <request>` with messages before it: the question says how many would run in plan
     * mode; clearing the queue is offered only when they all wait in the daemon's queue.
     */
    @Test
    fun thePlanAheadQuestionOffersToClearTheQueueOnlyWhenItCan() {
        val chosen = mutableListOf<String>()
        var ahead by androidx.compose.runtime.mutableStateOf(SendConfirmation.PlanAhead(ahead = 2, canClear = true))
        content(scroll = false) {
            PlanAheadDialog(
                ahead,
                onSend = { chosen += "send" },
                onClearAndSend = if (ahead.canClear) ({ chosen += "clear" }) else null,
                onDismiss = { chosen += "cancel" },
            )
        }
        compose.onNodeWithText("先に待っているメッセージもプランモードになります").assertIsDisplayed()
        compose.onNodeWithText("この依頼より先に始まる 2 件", substring = true).assertIsDisplayed()
        compose.onNodeWithText("キューを消去して送る").performClick()
        compose.onNodeWithText("このまま送る").performClick()
        assertEquals(listOf("clear", "send"), chosen)
        ahead = SendConfirmation.PlanAhead(ahead = 1, canClear = false)
        compose.waitForIdle()
        compose.onNodeWithText("キューを消去して送る").assertDoesNotExist()
        compose.onNodeWithText("この依頼より先に始まる 1 件", substring = true).assertIsDisplayed()
    }

    /**
     * The model sheet opened by `/model <id>` whose model lacks the thread's effort: that model is
     * chosen, the effort is to be picked from its levels, and applying sends both.
     */
    @Test
    fun theModelSheetOpensWithTheTypedModelAndAsksForAnEffortItOffers() {
        val applied = mutableListOf<Pair<String?, String?>>()
        content(scroll = false) {
            ModelSheet(
                TestEngine.fakeHarness(),
                ThreadSettings(model = "large", effort = "high"),
                allowDefaultEffort = false,
                onApply = { model, effort, _ -> applied += model to effort },
                onDismiss = {},
                initialModel = "small",
            )
        }
        compose.onNodeWithText("このモデルで使える推論レベルを選んでください").assertIsDisplayed()
        compose.onNodeWithText("適用").assertIsNotEnabled()
        compose.onNodeWithText("High").assertDoesNotExist()
        compose.onNodeWithText("Low").performClick()
        compose.onNodeWithText("適用").assertIsEnabled().performClick()
        assertEquals(listOf<Pair<String?, String?>>("small" to "low"), applied)
    }

    @Test
    fun composerTextFromTheHarnessWaitsAsAnOffer() {
        val chosen = mutableListOf<String>()
        content {
            ComposerInsertOffer("run the tests", composerEmpty = false, onReplace = { chosen += "replace" }, onAppend = { chosen += "append" }, onDismiss = { chosen += "dismiss" })
            ProjectTrustBanner("pi", onTrust = { chosen += "trust" }, onDistrust = { chosen += "distrust" })
        }
        compose.onNodeWithText("エージェントが入力欄に入れる文を送ってきました").assertIsDisplayed()
        compose.onNodeWithText("後ろに追加").performClick()
        compose.onNodeWithText("置き換える").performClick()
        compose.onNodeWithText("閉じる").performClick()
        compose.onNodeWithText("このプロジェクトを信頼しますか？（pi）").assertIsDisplayed()
        compose.onNodeWithText("信頼しない").performClick()
        compose.onNodeWithText("信頼する").performClick()
        assertEquals(listOf("append", "replace", "dismiss", "distrust", "trust"), chosen)
    }
}

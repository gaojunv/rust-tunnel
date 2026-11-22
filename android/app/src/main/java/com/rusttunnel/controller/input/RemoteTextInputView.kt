package com.rusttunnel.controller.input

import android.content.Context
import android.graphics.Color
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.CompletionInfo
import android.view.inputmethod.CorrectionInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import com.rusttunnel.controller.HostController

/**
 * 软键盘捕获视图（1×1、透明、可聚焦，不做实际文本编辑）。
 *
 * 只转发**已提交**的文本（`commitText`）；输入法的组合过程（拼音/候选）
 * 一律吞掉，等组合完成后再整段发送——避免远端被逐字蹦出来的拼音污染。
 *
 * 句柄类按键（退格/回车/方向键）映射成一次「按下+抬起」发送。
 */
class RemoteTextInputView(context: Context) : View(context) {
    private val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAutofill = IMPORTANT_FOR_AUTOFILL_NO
        setBackgroundColor(Color.TRANSPARENT)
    }

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_ACTION_NONE
        return object : BaseInputConnection(this, false) {
            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                if (!text.isNullOrEmpty()) {
                    HostController.sendText(text.toString())
                }
                return true
            }

            override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
                // 组合中（拼音、手写候选中）不发送；提交时走 commitText。
                return true
            }

            override fun finishComposingText(): Boolean = true

            override fun commitCompletion(text: CompletionInfo?): Boolean {
                if (text != null) HostController.sendText(text.text.toString())
                return true
            }

            override fun commitCorrection(correctionInfo: CorrectionInfo?): Boolean {
                if (correctionInfo != null) HostController.sendText(correctionInfo.newText.toString())
                return true
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                // 输入法的退格/回车/方向键只保证 ACTION_DOWN，远端若收到半对事件会卡住键位，
                // 因此这里统一按「点按」发送完整的按下+抬起。
                if (event.action == KeyEvent.ACTION_DOWN) {
                    HostController.sendAndroidKeyTap(event.keyCode)
                }
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                repeat(beforeLength.coerceIn(0, 32)) {
                    HostController.sendAndroidKeyTap(KeyEvent.KEYCODE_DEL)
                }
                return true
            }

            override fun performEditorAction(actionCode: Int): Boolean {
                HostController.sendAndroidKeyTap(KeyEvent.KEYCODE_ENTER)
                return true
            }
        }
    }

    /** 弹出软键盘（工具条「键盘」按钮）。 */
    fun showKeyboard() {
        requestFocus()
        post {
            imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
        }
    }

    fun hideKeyboard() {
        imm.hideSoftInputFromWindow(windowToken, 0)
    }

    fun isKeyboardVisible(): Boolean = imm.isActive(this)
}

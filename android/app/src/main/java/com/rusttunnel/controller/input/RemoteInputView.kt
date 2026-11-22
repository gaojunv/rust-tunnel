package com.rusttunnel.controller.input

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.PointF
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.min

/**
 * 覆盖在渲染 Surface 之上的透明手势层，把触摸翻译成远端的鼠标/滚轮事件。
 *
 * 手势映射（契约 §4）：
 * - 单指轻点 → 左键点按
 * - 单指拖动 → 左键拖动
 * - 长按 → 右键点按
 * - 双指上下滑 → 滚轮（1.0 = 一格）
 * - 双指轻点 → 右键点按
 * - 双指捏合 → **本机**缩放/平移（只变换 [bindSurface] 绑定的渲染视图，不发任何事件）
 *
 * 坐标换算：本视图与 Surface 同尺寸，Rust 按 contain 适配把远端画面画在中央；
 * 本视图再叠加本地缩放/平移（`scale`/`tx`/`ty`）。最终发给 Rust 的是**远端像素坐标**。
 */
@SuppressLint("ClickableViewAccessibility")
class RemoteInputView(context: Context) : View(context) {
    /** 手势回调（都由 [HostController] 直接转发给原生层）。 */
    interface Listener {
        fun onRemoteMove(x: Float, y: Float)
        fun onRemoteButton(button: Int, pressed: Boolean)
        fun onRemoteWheel(dy: Float)
    }

    var listener: Listener? = null

    var remoteWidth: Int = 0
        private set
    var remoteHeight: Int = 0
        private set

    /** 本地缩放变化回调（只用于界面角标，不影响远端）。 */
    var onZoomChanged: ((Float) -> Unit)? = null

    val zoomScale: Float
        get() = scale

    private var surfaceView: View? = null

    private var scale = 1f
    private var tx = 0f
    private var ty = 0f

    private val touchSlop = ViewConfiguration.get(context).scaledTouchSlop.toFloat()
    private val longPressTimeout = ViewConfiguration.getLongPressTimeout().toLong()
    private val wheelStep = 56f * resources.displayMetrics.density

    private enum class Mode { Idle, OneFinger, TwoFinger, Transform, Wheel, Ignore }

    private var mode = Mode.Idle
    private var downTime = 0L
    private var downX = 0f
    private var downY = 0f
    private var moved = false
    private var leftDown = false
    private var longPressFired = false
    private var suppressTap = false

    private var twoStartTime = 0L
    private var twoStartDist = 1f
    private val twoStartCentroid = PointF()
    private var lastCentroidY = 0f
    private var wheelAccum = 0f

    private var transformStartScale = 1f
    private val anchorSurface = PointF()

    private val longPressRunnable = Runnable {
        if (mode == Mode.OneFinger && !moved) {
            longPressFired = true
            sendRightClick(lastTouchX, lastTouchY)
        }
    }

    private var lastTouchX = 0f
    private var lastTouchY = 0f

    init {
        isClickable = true
        isFocusable = false
        setBackgroundColor(0x00000000)
    }

    /** 绑定被本地缩放的渲染视图（通常就是 `SurfaceView`）。 */
    fun bindSurface(view: View) {
        surfaceView = view
        view.pivotX = 0f
        view.pivotY = 0f
        applyTransform()
    }

    /** 远端画面逻辑尺寸变化时调用（`onFrameInfo`）。 */
    fun setRemoteSize(width: Int, height: Int) {
        if (remoteWidth == width && remoteHeight == height) return
        remoteWidth = width.coerceAtLeast(0)
        remoteHeight = height.coerceAtLeast(0)
        clampPan()
        applyTransform()
    }

    fun resetZoom() {
        scale = 1f
        tx = 0f
        ty = 0f
        applyTransform()
        onZoomChanged?.invoke(scale)
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        clampPan()
        applyTransform()
    }

    // ── 手势 ────────────────────────────────────────────────────────────────

    override fun onTouchEvent(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> handleDown(event)
            MotionEvent.ACTION_POINTER_DOWN -> if (event.pointerCount == 2) handleSecondDown(event)
            MotionEvent.ACTION_MOVE -> handleMove(event)
            MotionEvent.ACTION_POINTER_UP -> if (event.pointerCount == 2) handlePointerUp(event)
            MotionEvent.ACTION_UP -> handleUp(event)
            MotionEvent.ACTION_CANCEL -> cancelGesture()
        }
        return true
    }

    private fun handleDown(event: MotionEvent) {
        mode = Mode.OneFinger
        downTime = System.currentTimeMillis()
        downX = event.x
        downY = event.y
        lastTouchX = event.x
        lastTouchY = event.y
        moved = false
        leftDown = false
        longPressFired = false
        suppressTap = false
        removeCallbacks(longPressRunnable)
        postDelayed(longPressRunnable, longPressTimeout)
        sendMove(event.x, event.y)
    }

    private fun handleSecondDown(event: MotionEvent) {
        removeCallbacks(longPressRunnable)
        if (leftDown) {
            leftDown = false
            listener?.onRemoteButton(BUTTON_LEFT, false)
        }
        val centroid = centroid(event)
        mode = Mode.TwoFinger
        moved = false
        suppressTap = false
        twoStartTime = System.currentTimeMillis()
        twoStartDist = spacing(event)
        twoStartCentroid.set(centroid.x, centroid.y)
        lastCentroidY = centroid.y
        wheelAccum = 0f
        transformStartScale = scale
        anchorSurface.set((centroid.x - tx) / scale, (centroid.y - ty) / scale)
    }

    private fun handleMove(event: MotionEvent) {
        when (mode) {
            Mode.OneFinger -> {
                lastTouchX = event.x
                lastTouchY = event.y
                if (!moved && hypot(event.x - downX, event.y - downY) > touchSlop) {
                    moved = true
                    removeCallbacks(longPressRunnable)
                }
                if (moved && !longPressFired && !leftDown) {
                    leftDown = true
                    listener?.onRemoteButton(BUTTON_LEFT, true)
                }
                if (!longPressFired) {
                    sendMove(event.x, event.y)
                }
            }

            Mode.TwoFinger -> {
                val distance = spacing(event)
                val centroid = centroid(event)
                val distanceDelta = abs(distance - twoStartDist)
                val moveDelta = hypot(
                    centroid.x - twoStartCentroid.x,
                    centroid.y - twoStartCentroid.y,
                )
                if (distanceDelta > touchSlop * 1.2f) {
                    mode = Mode.Transform
                    moved = true
                    handleTransform(event)
                } else if (moveDelta > touchSlop) {
                    mode = Mode.Wheel
                    moved = true
                    wheelAccum = 0f
                    lastCentroidY = centroid.y
                }
            }

            Mode.Transform -> handleTransform(event)
            Mode.Wheel -> handleWheel(event)
            Mode.Idle, Mode.Ignore -> Unit
        }
    }

    private fun handleTransform(event: MotionEvent) {
        val distance = spacing(event)
        val centroid = centroid(event)
        val factor = (distance / twoStartDist).coerceIn(0.05f, 20f)
        val newScale = (transformStartScale * factor).coerceIn(MIN_SCALE, MAX_SCALE)
        scale = newScale
        tx = centroid.x - anchorSurface.x * newScale
        ty = centroid.y - anchorSurface.y * newScale
        clampPan()
        applyTransform()
        onZoomChanged?.invoke(scale)
    }

    private fun handleWheel(event: MotionEvent) {
        val centroid = centroid(event)
        val delta = lastCentroidY - centroid.y
        lastCentroidY = centroid.y
        wheelAccum += delta / wheelStep
        val notches = wheelAccum.toInt()
        if (notches != 0) {
            wheelAccum -= notches
            listener?.onRemoteWheel(notches.toFloat())
        }
    }

    private fun handlePointerUp(event: MotionEvent) {
        if (mode == Mode.TwoFinger && !moved &&
            System.currentTimeMillis() - twoStartTime < TWO_FINGER_TAP_TIMEOUT_MS
        ) {
            val centroid = centroid(event)
            sendRightClick(centroid.x, centroid.y)
            suppressTap = true
        }
        mode = Mode.Ignore
    }

    private fun handleUp(event: MotionEvent) {
        removeCallbacks(longPressRunnable)
        if (mode == Mode.OneFinger) {
            if (leftDown) {
                leftDown = false
                listener?.onRemoteButton(BUTTON_LEFT, false)
            } else if (
                !moved &&
                !longPressFired &&
                !suppressTap &&
                System.currentTimeMillis() - downTime < longPressTimeout
            ) {
                performClick()
                sendLeftClick(event.x, event.y)
            }
        }
        mode = Mode.Idle
        moved = false
        longPressFired = false
        suppressTap = false
    }

    private fun cancelGesture() {
        removeCallbacks(longPressRunnable)
        if (leftDown) {
            leftDown = false
            listener?.onRemoteButton(BUTTON_LEFT, false)
        }
        mode = Mode.Idle
        moved = false
        longPressFired = false
        suppressTap = false
    }

    override fun performClick(): Boolean {
        super.performClick()
        return true
    }

    // ── 坐标换算与本地变换 ──────────────────────────────────────────────────

    /**
     * 远端画面在视图里的 contain 矩形：`[offsetX, offsetY, baseScale]`。
     * Rust 的渲染器按同一规则把远端画面居中缩放。
     */
    private fun contentRect(): FloatArray? {
        if (remoteWidth <= 0 || remoteHeight <= 0 || width <= 0 || height <= 0) return null
        val base = min(width.toFloat() / remoteWidth, height.toFloat() / remoteHeight)
        val drawWidth = remoteWidth * base
        val drawHeight = remoteHeight * base
        return floatArrayOf(
            (width - drawWidth) / 2f,
            (height - drawHeight) / 2f,
            base,
        )
    }

    /** 视图坐标（未叠加本地缩放前）→ 远端像素坐标。 */
    private fun toRemote(x: Float, y: Float): Pair<Float, Float>? {
        val rect = contentRect() ?: return null
        val surfaceX = (x - tx) / scale
        val surfaceY = (y - ty) / scale
        val remoteX = ((surfaceX - rect[0]) / rect[2]).coerceIn(0f, remoteWidth - 1f)
        val remoteY = ((surfaceY - rect[1]) / rect[2]).coerceIn(0f, remoteHeight - 1f)
        return remoteX to remoteY
    }

    private fun sendMove(x: Float, y: Float) {
        val (remoteX, remoteY) = toRemote(x, y) ?: return
        listener?.onRemoteMove(remoteX, remoteY)
    }

    private fun sendLeftClick(x: Float, y: Float) {
        sendMove(x, y)
        listener?.onRemoteButton(BUTTON_LEFT, true)
        listener?.onRemoteButton(BUTTON_LEFT, false)
    }

    private fun sendRightClick(x: Float, y: Float) {
        sendMove(x, y)
        listener?.onRemoteButton(BUTTON_RIGHT, true)
        listener?.onRemoteButton(BUTTON_RIGHT, false)
    }

    private fun applyTransform() {
        val view = surfaceView ?: return
        view.scaleX = scale
        view.scaleY = scale
        view.translationX = tx
        view.translationY = ty
        invalidate()
    }

    /** 限制平移，保证画面不会被拖出视图（1× 时正好居中）。 */
    private fun clampPan() {
        val rect = contentRect() ?: return
        val offsetX = rect[0]
        val offsetY = rect[1]
        val base = rect[2]
        val drawWidth = remoteWidth * base
        val drawHeight = remoteHeight * base
        val viewWidth = width.toFloat()
        val viewHeight = height.toFloat()
        val scaledWidth = drawWidth * scale
        val scaledHeight = drawHeight * scale

        tx = if (scaledWidth <= viewWidth) {
            viewWidth / 2f - (offsetX + drawWidth / 2f) * scale
        } else {
            tx.coerceIn(viewWidth - (offsetX + drawWidth) * scale, -offsetX * scale)
        }
        ty = if (scaledHeight <= viewHeight) {
            viewHeight / 2f - (offsetY + drawHeight / 2f) * scale
        } else {
            ty.coerceIn(viewHeight - (offsetY + drawHeight) * scale, -offsetY * scale)
        }
    }

    private fun spacing(event: MotionEvent): Float {
        if (event.pointerCount < 2) return 1f
        val dx = event.getX(0) - event.getX(1)
        val dy = event.getY(0) - event.getY(1)
        return hypot(dx, dy).coerceAtLeast(1f)
    }

    private fun centroid(event: MotionEvent): PointF {
        if (event.pointerCount == 0) return PointF(0f, 0f)
        var sumX = 0f
        var sumY = 0f
        for (index in 0 until event.pointerCount) {
            sumX += event.getX(index)
            sumY += event.getY(index)
        }
        return PointF(sumX / event.pointerCount, sumY / event.pointerCount)
    }

    companion object {
        const val BUTTON_LEFT = 0
        const val BUTTON_RIGHT = 1
        const val BUTTON_MIDDLE = 2

        private const val MIN_SCALE = 1f
        private const val MAX_SCALE = 8f
        private const val TWO_FINGER_TAP_TIMEOUT_MS = 400L
    }
}

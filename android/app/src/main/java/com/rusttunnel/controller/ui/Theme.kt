package com.rusttunnel.controller.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val DarkColors = darkColorScheme(
    primary = Color(0xFF7CB8FF),
    onPrimary = Color(0xFF001B33),
    primaryContainer = Color(0xFF123A5C),
    onPrimaryContainer = Color(0xFFD3E7FF),
    secondary = Color(0xFF9FCBFF),
    onSecondary = Color(0xFF001B33),
    background = Color(0xFF0E1116),
    onBackground = Color(0xFFE5EAF1),
    surface = Color(0xFF161B22),
    onSurface = Color(0xFFE5EAF1),
    surfaceVariant = Color(0xFF232A34),
    onSurfaceVariant = Color(0xFFB8C2CF),
    outline = Color(0xFF46505C),
    error = Color(0xFFFF8A80),
    onError = Color(0xFF2A0000),
    errorContainer = Color(0xFF5C1A17),
    onErrorContainer = Color(0xFFFFDAD6),
)

private val LightColors = lightColorScheme(
    primary = Color(0xFF1F5FAF),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFD5E4FF),
    onPrimaryContainer = Color(0xFF001B3B),
    secondary = Color(0xFF3F6B9C),
    background = Color(0xFFF7F9FC),
    onBackground = Color(0xFF191C20),
    surface = Color.White,
    onSurface = Color(0xFF191C20),
    surfaceVariant = Color(0xFFE1E7EF),
    onSurfaceVariant = Color(0xFF444A52),
    outline = Color(0xFF74808F),
    error = Color(0xFFB3261E),
    onError = Color.White,
)

@Composable
fun RustTunnelTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    MaterialTheme(
        colorScheme = if (darkTheme) DarkColors else LightColors,
        content = content,
    )
}
